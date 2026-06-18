//! Liveness / failure detection (architecture §8): a **phi-accrual** failure
//! detector over heartbeat/gossip intervals, plus **SWIM-style indirect
//! probing**, layered on the existing libp2p gossip overlay.
//!
//! The two mechanisms compose:
//!  * The [`PhiDetector`] (in `p2p-trust`) turns the stream of heartbeats /
//!    gossip capability-ad re-publishes for a peer into a continuously-rising
//!    suspicion level `φ`. A peer whose `φ` crosses the configured conviction
//!    threshold is *suspected*.
//!  * Before a suspected peer is actually declared dead and dropped from
//!    candidate selection, **SWIM indirect probing** asks `k` random other peers
//!    to probe it ([`swim_confirm`]). A single bad link (the suspecter ↔ suspect
//!    path) is thus not enough to evict a peer that the rest of the swarm can
//!    still reach — this is the classic SWIM false-positive reduction.
//!
//! [`LivenessView`] is the shared, bounded state the coordinator consults during
//! candidate selection ([`LivenessView::is_excluded`]); [`LivenessFilteredDiscovery`]
//! is a drop-in [`Discovery`] wrapper that applies it. Everything is config-driven
//! (`[liveness]`) and **off-path by default**: a coordinator with no liveness
//! view wired behaves exactly as before.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use p2p_config::LivenessConfig;
use p2p_proto::NodeId;
use p2p_trust::PhiDetector;
use rand::seq::SliceRandom;

use crate::discovery::{Candidate, CandidateFilter, Discovery};

/// A peer's directly-probeable health, used by SWIM.
#[async_trait]
pub trait Prober: Send + Sync {
    /// Directly probe `target`; `true` = reachable/alive.
    async fn probe(&self, target: &NodeId) -> bool;
}

/// Ask another peer to probe a target on our behalf (SWIM indirect probe).
#[async_trait]
pub trait IndirectProber: Send + Sync {
    /// Ask `relay` to probe `target`; `true` = the relay reports `target` alive.
    async fn probe_via(&self, relay: &NodeId, target: &NodeId) -> bool;
}

/// Outcome of a SWIM liveness confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwimVerdict {
    /// Confirmed reachable — directly or via an indirect probe ("rescued").
    Alive,
    /// Unreachable directly and via all `k` indirect probes — declared dead.
    Dead,
}

/// SWIM failure confirmation for a *suspected* peer (architecture §8).
///
/// 1. Direct-probe `target` (bounded by `swim.probe_timeout_ms`). Alive ⇒ done.
/// 2. Otherwise ask up to `swim.indirect_probe_count` (`k`) random `relays` to
///    probe it (each bounded by `swim.indirect_probe_timeout_ms`), concurrently.
///    If **any** relay reports it alive, the peer is **rescued** (`Alive`).
/// 3. If the direct probe and all indirect probes fail, it is `Dead`.
///
/// This is the false-positive reducer: a peer the suspecter can't reach but the
/// rest of the swarm can is kept, not evicted.
pub async fn swim_confirm(
    target: &NodeId,
    relays: &[NodeId],
    prober: &dyn Prober,
    indirect: &dyn IndirectProber,
    cfg: &p2p_config::SwimConfig,
) -> SwimVerdict {
    let direct_to = Duration::from_millis(cfg.probe_timeout_ms.max(1));
    if timeout_true(direct_to, prober.probe(target)).await {
        return SwimVerdict::Alive;
    }
    if !cfg.enabled || cfg.indirect_probe_count == 0 {
        return SwimVerdict::Dead;
    }

    // Pick up to k random relays (excluding the target itself).
    let mut pool: Vec<&NodeId> = relays.iter().filter(|r| *r != target).collect();
    pool.shuffle(&mut rand::thread_rng());
    pool.truncate(cfg.indirect_probe_count);
    if pool.is_empty() {
        return SwimVerdict::Dead;
    }

    let indirect_to = Duration::from_millis(cfg.indirect_probe_timeout_ms.max(1));
    let probes = pool.into_iter().map(|relay| {
        let relay = relay.clone();
        async move { timeout_true(indirect_to, indirect.probe_via(&relay, target)).await }
    });
    // Any indirect success rescues the peer.
    let results = futures_util::future::join_all(probes).await;
    if results.into_iter().any(|alive| alive) {
        SwimVerdict::Alive
    } else {
        SwimVerdict::Dead
    }
}

/// Await `fut` with a timeout, treating a timeout as `false` (probe failed).
async fn timeout_true(dur: Duration, fut: impl std::future::Future<Output = bool>) -> bool {
    matches!(tokio::time::timeout(dur, fut).await, Ok(true))
}

/// A SWIM-confirmed override layered on top of the phi suspicion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Forced {
    /// No override — phi alone decides.
    None,
    /// SWIM rescued the peer (reachable via the swarm); keep it selectable.
    Alive,
    /// SWIM confirmed the peer dead; exclude it from selection.
    Dead,
}

struct PeerLiveness {
    phi: PhiDetector,
    forced: Forced,
}

/// Shared, per-peer liveness state consulted during candidate selection.
///
/// Bounded by the number of distinct peers observed; entries are cheap (a small
/// ring of intervals). `now_ms` is any consistent millisecond clock.
pub struct LivenessView {
    cfg: LivenessConfig,
    inner: Mutex<HashMap<NodeId, PeerLiveness>>,
}

impl LivenessView {
    pub fn new(cfg: LivenessConfig) -> Self {
        Self {
            cfg,
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Record a heartbeat / gossip re-publish for `node` at `now_ms`. Resets any
    /// SWIM override (a fresh signal is the ground truth).
    pub fn heartbeat(&self, node: &NodeId, now_ms: u64) {
        let mut g = self.inner.lock().unwrap();
        let entry = g.entry(node.clone()).or_insert_with(|| PeerLiveness {
            phi: PhiDetector::new(&self.cfg.phi),
            forced: Forced::None,
        });
        entry.phi.heartbeat(now_ms);
        entry.forced = Forced::None;
    }

    /// The current suspicion level `φ` for `node` (0 if unknown / phi disabled).
    pub fn phi(&self, node: &NodeId, now_ms: u64) -> f64 {
        if !self.cfg.phi.enabled {
            return 0.0;
        }
        let g = self.inner.lock().unwrap();
        g.get(node).map(|p| p.phi.phi(now_ms)).unwrap_or(0.0)
    }

    /// Whether phi alone currently *suspects* `node` (φ over the threshold). An
    /// unknown peer (no heartbeats yet) is never suspected.
    pub fn is_suspect(&self, node: &NodeId, now_ms: u64) -> bool {
        if !self.cfg.phi.enabled {
            return false;
        }
        let g = self.inner.lock().unwrap();
        match g.get(node) {
            Some(p) => p.phi.has_heartbeat() && !p.phi.is_available(now_ms),
            None => false,
        }
    }

    /// Whether `node` should be **excluded** from candidate selection: a
    /// SWIM-confirmed dead peer, or a phi-suspect peer SWIM has not rescued.
    pub fn is_excluded(&self, node: &NodeId, now_ms: u64) -> bool {
        let g = self.inner.lock().unwrap();
        match g.get(node) {
            Some(p) => match p.forced {
                Forced::Dead => true,
                Forced::Alive => false,
                Forced::None => p.phi.has_heartbeat() && !p.phi.is_available(now_ms),
            },
            None => false,
        }
    }

    /// Record a SWIM verdict for `node` (sticky until the next heartbeat).
    pub fn apply_swim(&self, node: &NodeId, verdict: SwimVerdict) {
        let mut g = self.inner.lock().unwrap();
        let entry = g.entry(node.clone()).or_insert_with(|| PeerLiveness {
            phi: PhiDetector::new(&self.cfg.phi),
            forced: Forced::None,
        });
        entry.forced = match verdict {
            SwimVerdict::Alive => Forced::Alive,
            SwimVerdict::Dead => Forced::Dead,
        };
    }

    /// Run SWIM confirmation for `node` (only if currently phi-suspect) and
    /// record the verdict. Returns `true` if the peer ends up considered alive.
    /// A non-suspect / unknown peer is left untouched and reported alive.
    pub async fn confirm_with_swim(
        &self,
        node: &NodeId,
        relays: &[NodeId],
        prober: &dyn Prober,
        indirect: &dyn IndirectProber,
        now_ms: u64,
    ) -> bool {
        if !self.cfg.swim.enabled || !self.is_suspect(node, now_ms) {
            return !self.is_excluded(node, now_ms);
        }
        let verdict = swim_confirm(node, relays, prober, indirect, &self.cfg.swim).await;
        self.apply_swim(node, verdict);
        verdict == SwimVerdict::Alive
    }

    pub fn config(&self) -> &LivenessConfig {
        &self.cfg
    }
}

/// A [`Discovery`] wrapper that drops peers the [`LivenessView`] currently
/// excludes (phi-convicted and not SWIM-rescued). Drop-in over any inner
/// discovery; uses a monotonic-ish wall clock (`now_ms`) for the phi query.
pub struct LivenessFilteredDiscovery {
    inner: std::sync::Arc<dyn Discovery>,
    view: std::sync::Arc<LivenessView>,
}

impl LivenessFilteredDiscovery {
    pub fn new(inner: std::sync::Arc<dyn Discovery>, view: std::sync::Arc<LivenessView>) -> Self {
        Self { inner, view }
    }
}

/// Current wall clock in unix milliseconds (shared liveness time base).
pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[async_trait]
impl Discovery for LivenessFilteredDiscovery {
    async fn find_candidates(&self, want: usize, filter: CandidateFilter) -> Vec<Candidate> {
        // Over-fetch a little so excluding dead peers still yields `want`.
        let raw = self
            .inner
            .find_candidates(want.saturating_mul(2).max(want), filter)
            .await;
        let now = now_ms();
        let mut out: Vec<Candidate> = raw
            .into_iter()
            .filter(|c| match &c.node_id {
                Some(id) => !self.view.is_excluded(id, now),
                None => true, // unknown id can't be liveness-tracked → keep
            })
            .collect();
        out.truncate(want);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn cfg() -> LivenessConfig {
        let mut c = LivenessConfig::default();
        c.phi.first_interval_ms = 1_000.0;
        c.phi.convict_threshold = 8.0;
        c.swim.indirect_probe_count = 3;
        c
    }

    fn node(s: &str) -> NodeId {
        NodeId(s.to_string())
    }

    // A prober keyed by which nodes are directly reachable.
    struct FakeProber {
        reachable: Vec<NodeId>,
    }
    #[async_trait]
    impl Prober for FakeProber {
        async fn probe(&self, target: &NodeId) -> bool {
            self.reachable.contains(target)
        }
    }

    // An indirect prober: a relay can reach the target iff (relay, target) is allowed.
    struct FakeIndirect {
        // (relay, target) pairs that succeed
        allowed: Vec<(NodeId, NodeId)>,
    }
    #[async_trait]
    impl IndirectProber for FakeIndirect {
        async fn probe_via(&self, relay: &NodeId, target: &NodeId) -> bool {
            self.allowed.iter().any(|(r, t)| r == relay && t == target)
        }
    }

    #[test]
    fn phi_silence_marks_node_suspect_and_excluded() {
        let view = LivenessView::new(cfg());
        let n = node("b3:w");
        for i in 0..10u64 {
            view.heartbeat(&n, i * 1_000);
        }
        let last = 9 * 1_000;
        // Fresh: available.
        assert!(!view.is_excluded(&n, last + 500));
        // Long silence → suspect → excluded.
        assert!(view.is_suspect(&n, last + 60_000));
        assert!(view.is_excluded(&n, last + 60_000));
    }

    #[tokio::test]
    async fn swim_indirect_probe_rescues_a_peer_reachable_via_peers() {
        let view = LivenessView::new(cfg());
        let target = node("b3:target");
        // Make the target phi-suspect via silence.
        for i in 0..10u64 {
            view.heartbeat(&target, i * 1_000);
        }
        let now = 9 * 1_000 + 60_000;
        assert!(
            view.is_excluded(&target, now),
            "silent target is excluded before SWIM"
        );

        // Direct probe fails, but relay r2 can reach the target.
        let prober = FakeProber { reachable: vec![] };
        let indirect = FakeIndirect {
            allowed: vec![(node("b3:r2"), target.clone())],
        };
        let relays = vec![node("b3:r1"), node("b3:r2"), node("b3:r3")];
        let alive = view
            .confirm_with_swim(&target, &relays, &prober, &indirect, now)
            .await;
        assert!(alive, "SWIM should rescue a peer reachable via a relay");
        assert!(
            !view.is_excluded(&target, now),
            "rescued peer is selectable again"
        );
    }

    #[tokio::test]
    async fn swim_declares_dead_when_no_path_exists() {
        let view = LivenessView::new(cfg());
        let target = node("b3:gone");
        for i in 0..10u64 {
            view.heartbeat(&target, i * 1_000);
        }
        let now = 9 * 1_000 + 60_000;
        let prober = FakeProber { reachable: vec![] };
        let indirect = FakeIndirect { allowed: vec![] }; // no relay can reach it
        let relays = vec![node("b3:r1"), node("b3:r2")];
        let alive = view
            .confirm_with_swim(&target, &relays, &prober, &indirect, now)
            .await;
        assert!(!alive);
        assert!(
            view.is_excluded(&target, now),
            "unreachable peer stays excluded"
        );
    }

    #[tokio::test]
    async fn liveness_filtered_discovery_drops_excluded_peers() {
        use crate::discovery::StaticDiscovery;
        use p2p_proto::{AttestationLevel, DataClass};

        let dead = node("b3:dead");
        let view = Arc::new(LivenessView::new(cfg()));
        // Mark `dead` convicted directly.
        view.apply_swim(&dead, SwimVerdict::Dead);

        let mut c_dead = Candidate::new(Some(dead.clone()), "127.0.0.1:9001".parse().unwrap());
        c_dead.advertised_level = Some(AttestationLevel::L0);
        let mut c_ok = Candidate::new(Some(node("b3:ok")), "127.0.0.1:9002".parse().unwrap());
        c_ok.advertised_level = Some(AttestationLevel::L0);

        let inner = Arc::new(StaticDiscovery::new(vec![c_dead, c_ok], 16));
        let disc = LivenessFilteredDiscovery::new(inner, Arc::clone(&view));
        let got = disc
            .find_candidates(
                16,
                CandidateFilter {
                    data_class: DataClass::Public,
                    min_attestation: AttestationLevel::L0,
                    network: None,
                    groups: vec![],
                    regions: vec![],
                    fail_closed_labels: false,
                },
            )
            .await;
        assert!(got.iter().all(|c| c.node_id.as_ref() != Some(&dead)));
        assert!(got
            .iter()
            .any(|c| c.node_id.as_ref() == Some(&node("b3:ok"))));
    }
}
