//! Runtime anti-abuse helpers consulted on the hot path (ARCHITECTURE "Abuse
//! resistance"): an in-memory deny-list and a per-requester free-job rate
//! limiter. Pure scoring/classification primitives live in `p2p_trust::antiabuse`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;

use p2p_config::{BlockKind, BlocklistStore, CostGateConfig};
use p2p_proto::{AbuseSignal, NodeId};
use p2p_trust::{now_ts, verify_abuse_signal};
use tracing::debug;

/// Pre-flight cost gate (ARCHITECTURE "Abuse resistance"): decide whether an
/// offer/job is admissible BEFORE execution, using the advertised cost hint
/// (rows) and/or a pre-flight estimated peak working set. Returns `Some(reason)`
/// to **reject** (the offer is declined, not failed — no receipt, no score
/// effect), or `None` to admit. A heavy query is declined up front rather than
/// run to an OOM that would unfairly look like provider failure.
///
/// * `cost_hint_rows` — the requester's advertised estimated rows scanned.
/// * `estimated_peak_bytes` — an optional locally-estimated peak working set.
/// * `per_job_memory_bytes` — this worker's per-job memory lease.
pub fn cost_gate_reason(
    cfg: &CostGateConfig,
    cost_hint_rows: Option<u64>,
    estimated_peak_bytes: Option<u64>,
    per_job_memory_bytes: u64,
) -> Option<String> {
    if cfg.max_cost_hint_rows > 0 {
        if let Some(rows) = cost_hint_rows {
            if rows > cfg.max_cost_hint_rows {
                return Some(format!(
                    "over budget: estimated {rows} rows exceeds the worker's max_cost_hint_rows ({})",
                    cfg.max_cost_hint_rows
                ));
            }
        }
    }
    if let Some(peak) = estimated_peak_bytes {
        let factor = if cfg.max_working_set_factor > 0.0 {
            cfg.max_working_set_factor
        } else {
            1.0
        };
        let cap = (per_job_memory_bytes as f64 * factor) as u64;
        if cap > 0 && peak > cap {
            return Some(format!(
                "over budget: estimated peak working set {peak} bytes exceeds budget {cap} bytes"
            ));
        }
    }
    None
}

/// In-memory deny-list consulted by the coordinator (candidate filtering /
/// auto-block) and the worker (refuse offers from blocked requesters). Each node
/// keeps its OWN list — there is no central authority. Optionally write-through
/// to a persisted [`BlocklistStore`] so SQL `p2p_blocklist()` reflects runtime
/// auto-blocks and they survive restart.
pub struct Blocklist {
    inner: Mutex<HashSet<String>>,
    store: Option<BlocklistStore>,
}

impl Default for Blocklist {
    fn default() -> Self {
        Self {
            inner: Mutex::new(HashSet::new()),
            store: None,
        }
    }
}

impl Blocklist {
    /// An empty in-memory deny-list (no persistence).
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed from a persisted [`BlocklistStore`] and keep writing auto-blocks
    /// back to it. Existing entries are loaded into the in-memory set.
    pub fn with_store(store: BlocklistStore) -> Self {
        let mut set = HashSet::new();
        if let Ok(entries) = store.list() {
            for e in entries {
                set.insert(e.id);
            }
        }
        Self {
            inner: Mutex::new(set),
            store: Some(store),
        }
    }

    /// Whether `id` (a node_id or wallet string) is currently blocked.
    pub fn is_blocked(&self, id: &str) -> bool {
        self.inner.lock().unwrap().contains(id)
    }

    /// Block an id. Idempotent. Writes through to the persisted store if any.
    pub fn block(&self, id: &str, kind: BlockKind, reason: &str, source: &str) {
        let id = id.trim();
        if id.is_empty() {
            return;
        }
        let inserted = self.inner.lock().unwrap().insert(id.to_string());
        if inserted {
            if let Some(store) = &self.store {
                let _ = store.block(id, kind, reason, source, now_ts());
            }
            debug!(id, source, "blocklisted actor");
        }
    }

    /// Unblock an id.
    pub fn unblock(&self, id: &str) -> bool {
        let removed = self.inner.lock().unwrap().remove(id);
        if removed {
            if let Some(store) = &self.store {
                let _ = store.unblock(id);
            }
        }
        removed
    }

    /// Number of blocked ids.
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Honor a signed, gossiped abuse signal: if `honor` is set and the signal
    /// verifies, block the subject (node id and, if present, wallet). Returns
    /// whether the signal was acted upon. Unverified or un-honored signals are
    /// ignored (each node decides independently).
    pub fn apply_signed_signal(&self, signal: &AbuseSignal, honor: bool) -> bool {
        if !honor || !verify_abuse_signal(signal) {
            return false;
        }
        self.block(
            signal.subject_id.as_str(),
            BlockKind::NodeId,
            &format!(
                "abuse signal: {} (from {})",
                signal.reason, signal.reporter_id
            ),
            "gossip",
        );
        if let Some(w) = &signal.subject_wallet {
            self.block(w, BlockKind::Wallet, &signal.reason, "gossip");
        }
        true
    }
}

/// Per-requester-identity sliding-window rate limiter for FREE jobs (anti-spam).
/// Bounded: tracks at most `capacity` distinct identities (FIFO-evicted).
pub struct RateLimiter {
    max_per_window: u32,
    window_secs: u64,
    capacity: usize,
    inner: Mutex<RateInner>,
}

struct RateInner {
    hits: HashMap<NodeId, VecDeque<u64>>,
    order: VecDeque<NodeId>,
}

impl RateLimiter {
    pub fn new(max_per_window: u32, window_secs: u64, capacity: usize) -> Self {
        Self {
            max_per_window,
            window_secs: window_secs.max(1),
            capacity: capacity.max(1),
            inner: Mutex::new(RateInner {
                hits: HashMap::new(),
                order: VecDeque::new(),
            }),
        }
    }

    /// Record an attempt by `id` at `now_secs` and return whether it is allowed
    /// (i.e. it did not exceed `max_per_window` within the trailing window).
    /// When over the limit the attempt is **not** recorded (so a blocked spammer
    /// can't keep pushing the window forward).
    pub fn check_and_record(&self, id: &NodeId, now_secs: u64) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if !inner.hits.contains_key(id) {
            while inner.order.len() >= self.capacity {
                if let Some(evict) = inner.order.pop_front() {
                    inner.hits.remove(&evict);
                } else {
                    break;
                }
            }
            inner.hits.insert(id.clone(), VecDeque::new());
            inner.order.push_back(id.clone());
        }
        let window = self.window_secs;
        let max = self.max_per_window as usize;
        let q = inner.hits.get_mut(id).expect("just inserted");
        while let Some(&front) = q.front() {
            if now_secs.saturating_sub(front) >= window {
                q.pop_front();
            } else {
                break;
            }
        }
        if q.len() >= max {
            return false;
        }
        q.push_back(now_secs);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocklist_block_unblock() {
        let bl = Blocklist::new();
        assert!(!bl.is_blocked("b3:x"));
        bl.block("b3:x", BlockKind::NodeId, "cheating", "manual");
        assert!(bl.is_blocked("b3:x"));
        assert_eq!(bl.len(), 1);
        assert!(bl.unblock("b3:x"));
        assert!(!bl.is_blocked("b3:x"));
    }

    #[test]
    fn rate_limiter_allows_burst_then_blocks_within_window() {
        let rl = RateLimiter::new(3, 60, 100);
        let id = NodeId("b3:spammer".into());
        assert!(rl.check_and_record(&id, 100));
        assert!(rl.check_and_record(&id, 101));
        assert!(rl.check_and_record(&id, 102));
        // 4th within the window is refused.
        assert!(!rl.check_and_record(&id, 103));
        // After the window slides, attempts are allowed again.
        assert!(rl.check_and_record(&id, 200));
    }

    #[test]
    fn rate_limiter_is_per_identity() {
        let rl = RateLimiter::new(1, 60, 100);
        let a = NodeId("b3:a".into());
        let b = NodeId("b3:b".into());
        assert!(rl.check_and_record(&a, 0));
        assert!(!rl.check_and_record(&a, 1));
        // A different requester has its own budget.
        assert!(rl.check_and_record(&b, 1));
    }

    #[test]
    fn cost_gate_rejects_over_budget_and_admits_within() {
        let mut cfg = CostGateConfig::default();
        cfg.enabled = true;
        cfg.max_cost_hint_rows = 1_000_000;
        cfg.max_working_set_factor = 1.0;
        // Within the row cap and no estimate ⇒ admit.
        assert!(cost_gate_reason(&cfg, Some(500_000), None, 1 << 30).is_none());
        // Over the row cap ⇒ reject.
        assert!(cost_gate_reason(&cfg, Some(5_000_000), None, 1 << 30).is_some());
        // Estimated peak over the per-job memory budget ⇒ reject.
        assert!(cost_gate_reason(&cfg, None, Some(2 << 30), 1 << 30).is_some());
        // Estimated peak within budget ⇒ admit.
        assert!(cost_gate_reason(&cfg, None, Some(512 << 20), 1 << 30).is_none());
    }

    #[test]
    fn signed_signal_blocks_only_when_honored_and_valid() {
        use ed25519_dalek::{Signer as _, SigningKey};
        use p2p_trust::sign_abuse_signal;
        use rand::rngs::OsRng;

        struct S(SigningKey);
        impl p2p_trust::receipt::Signer for S {
            fn sign_bytes(&self, m: &[u8]) -> [u8; 64] {
                self.0.sign(m).to_bytes()
            }
            fn public_key(&self) -> [u8; 32] {
                self.0.verifying_key().to_bytes()
            }
            fn node_id(&self) -> NodeId {
                NodeId::from_pubkey(&self.0.verifying_key().to_bytes())
            }
        }
        let signer = S(SigningKey::generate(&mut OsRng));
        let sig = sign_abuse_signal(NodeId("b3:bad".into()), None, "equivocation", 1, &signer);

        let bl = Blocklist::new();
        // Not honored ⇒ ignored.
        assert!(!bl.apply_signed_signal(&sig, false));
        assert!(!bl.is_blocked("b3:bad"));
        // Honored + valid ⇒ blocked.
        assert!(bl.apply_signed_signal(&sig, true));
        assert!(bl.is_blocked("b3:bad"));
    }
}
