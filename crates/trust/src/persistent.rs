//! Persistent reputation/receipt store (architecture §7.3, §18).
//!
//! [`RedbTrustStore`] is a durable implementation of the [`TrustStore`] trait
//! backed by the embedded, pure-Rust [`redb`] key-value store, so a node's
//! reputation trail (verified receipts, vouches, penalties) **survives a
//! restart**. The bounded, in-memory [`crate::InMemoryTrustStore`] remains the
//! default for tests and ephemeral nodes; this slots in behind the same trait
//! when `[trust].store_path` is configured.
//!
//! Scalability is preserved: per-worker observation history is capped
//! (`limits.receipt_cache_per_worker`) and the number of tracked workers is
//! capped with FIFO eviction (`limits.trust_store_capacity`), exactly like the
//! in-memory store — no unbounded growth on disk either.

use std::path::Path;
use std::sync::Arc;

use p2p_config::{LimitsConfig, TrustConfig};
use p2p_proto::{NodeId, Receipt};
use redb::{
    Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition,
};
use serde::{Deserialize, Serialize};

use crate::reputation::TrustStore;

/// `worker_id -> json(PersistWorker)`.
const WORKERS: TableDefinition<&str, &[u8]> = TableDefinition::new("worker_state");
/// `insertion_seq -> worker_id` (FIFO eviction order).
const ORDER: TableDefinition<u64, &str> = TableDefinition::new("worker_order");
/// Small key/value meta table (currently just the monotonic `next_seq`).
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");

/// Errors opening or operating the persistent store.
#[derive(Debug, thiserror::Error)]
pub enum TrustStoreError {
    #[error("trust store backend error: {0}")]
    Redb(String),
}

fn map_err<E: std::fmt::Display>(e: E) -> TrustStoreError {
    TrustStoreError::Redb(e.to_string())
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct PersistObs {
    ts: u64,
    correct: bool,
    weight: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistWorker {
    obs: Vec<PersistObs>,
    voucher_trust: f64,
    penalty: f64,
    /// Insertion sequence (for FIFO eviction).
    seq: u64,
}

impl Default for PersistWorker {
    fn default() -> Self {
        Self {
            obs: Vec::new(),
            voucher_trust: 0.0,
            penalty: 0.0,
            seq: 0,
        }
    }
}

/// A durable, bounded trust store backed by an embedded `redb` database.
pub struct RedbTrustStore {
    db: Arc<Database>,
    half_life_secs: f64,
    max_obs_per_worker: usize,
    max_workers: usize,
}

impl RedbTrustStore {
    /// Open (creating if absent) a persistent trust store at `path`. Bounds and
    /// the reputation half-life are taken from config — nothing is hard-coded.
    pub fn open(
        path: impl AsRef<Path>,
        trust: &TrustConfig,
        limits: &LimitsConfig,
    ) -> Result<Self, TrustStoreError> {
        let db = Database::create(path).map_err(map_err)?;
        // Ensure tables exist so read-only queries before any write don't fail.
        let wtxn = db.begin_write().map_err(map_err)?;
        {
            wtxn.open_table(WORKERS).map_err(map_err)?;
            wtxn.open_table(ORDER).map_err(map_err)?;
            wtxn.open_table(META).map_err(map_err)?;
        }
        wtxn.commit().map_err(map_err)?;
        Ok(Self {
            db: Arc::new(db),
            half_life_secs: trust.reputation_half_life_secs as f64,
            max_obs_per_worker: limits.receipt_cache_per_worker.max(1),
            max_workers: limits.trust_store_capacity.max(1),
        })
    }

    fn decay_weight(&self, age_secs: f64) -> f64 {
        if self.half_life_secs <= 0.0 {
            1.0
        } else {
            0.5f64.powf(age_secs.max(0.0) / self.half_life_secs)
        }
    }

    fn read_worker(&self, worker: &NodeId) -> Result<Option<PersistWorker>, TrustStoreError> {
        let rtxn = self.db.begin_read().map_err(map_err)?;
        let table = rtxn.open_table(WORKERS).map_err(map_err)?;
        let got = table.get(worker.0.as_str()).map_err(map_err)?;
        Ok(got.map(|g| serde_json::from_slice(g.value()).unwrap_or_default()))
    }

    /// Read-modify-write a worker's state inside a single write transaction,
    /// enforcing the per-worker observation cap and the global worker cap (FIFO).
    fn mutate(
        &self,
        worker: &NodeId,
        f: impl FnOnce(&mut PersistWorker),
    ) -> Result<(), TrustStoreError> {
        let key = worker.0.as_str();
        let wtxn = self.db.begin_write().map_err(map_err)?;
        {
            let mut workers = wtxn.open_table(WORKERS).map_err(map_err)?;
            let mut order = wtxn.open_table(ORDER).map_err(map_err)?;
            let mut meta = wtxn.open_table(META).map_err(map_err)?;

            let existing = workers
                .get(key)
                .map_err(map_err)?
                .map(|g| serde_json::from_slice::<PersistWorker>(g.value()).unwrap_or_default());

            let mut state = match existing {
                Some(s) => s,
                None => {
                    let next = meta
                        .get("next_seq")
                        .map_err(map_err)?
                        .map(|g| g.value())
                        .unwrap_or(0);
                    meta.insert("next_seq", next + 1).map_err(map_err)?;
                    order.insert(next, key).map_err(map_err)?;
                    PersistWorker {
                        seq: next,
                        ..Default::default()
                    }
                }
            };

            f(&mut state);
            while state.obs.len() > self.max_obs_per_worker {
                state.obs.remove(0);
            }
            let encoded = serde_json::to_vec(&state).map_err(map_err)?;
            workers.insert(key, encoded.as_slice()).map_err(map_err)?;

            // Evict oldest workers (lowest seq) while over capacity.
            while workers.len().map_err(map_err)? as usize > self.max_workers {
                let victim = {
                    let mut iter = order.iter().map_err(map_err)?;
                    match iter.next() {
                        Some(entry) => {
                            let (seq_guard, worker_guard) = entry.map_err(map_err)?;
                            Some((seq_guard.value(), worker_guard.value().to_string()))
                        }
                        None => None,
                    }
                };
                match victim {
                    Some((seq, victim_key)) => {
                        order.remove(seq).map_err(map_err)?;
                        workers.remove(victim_key.as_str()).map_err(map_err)?;
                    }
                    None => break,
                }
            }
        }
        wtxn.commit().map_err(map_err)?;
        Ok(())
    }
}

impl TrustStore for RedbTrustStore {
    fn record(&self, receipt: &Receipt) {
        // Only `Correct` + provable provider-fault verdicts touch reputation;
        // requester/job-caused and non-attributable verdicts are neutral (see
        // `Verdict::affects_reputation`, ARCHITECTURE "Abuse resistance").
        if !receipt.verdict.affects_reputation() {
            return;
        }
        let correct = receipt.verdict.is_correct();
        let ts = receipt.ts;
        let _ = self.mutate(&receipt.worker_id, |s| {
            s.obs.push(PersistObs {
                ts,
                correct,
                weight: 1.0,
            });
        });
    }

    fn reputation(&self, worker: &NodeId, now: u64) -> Option<f64> {
        let state = self.read_worker(worker).ok().flatten()?;
        if state.obs.is_empty() {
            return None;
        }
        let mut num = 0.0;
        let mut den = 0.0;
        for o in &state.obs {
            let age = now.saturating_sub(o.ts) as f64;
            let w = o.weight * self.decay_weight(age);
            den += w;
            if o.correct {
                num += w;
            }
        }
        if den == 0.0 {
            None
        } else {
            Some(num / den)
        }
    }

    fn observation_count(&self, worker: &NodeId) -> usize {
        self.read_worker(worker)
            .ok()
            .flatten()
            .map(|s| s.obs.len())
            .unwrap_or(0)
    }

    fn add_vouch(&self, worker: &NodeId, weight: f64) {
        let _ = self.mutate(worker, |s| s.voucher_trust += weight);
    }

    fn voucher_trust(&self, worker: &NodeId) -> f64 {
        self.read_worker(worker)
            .ok()
            .flatten()
            .map(|s| s.voucher_trust)
            .unwrap_or(0.0)
    }

    fn penalize(&self, worker: &NodeId, amount: f64) {
        let _ = self.mutate(worker, |s| s.penalty += amount);
    }

    fn penalty(&self, worker: &NodeId) -> f64 {
        self.read_worker(worker)
            .ok()
            .flatten()
            .map(|s| s.penalty)
            .unwrap_or(0.0)
    }

    fn tracked_workers(&self) -> usize {
        let res = (|| -> Result<u64, TrustStoreError> {
            let rtxn = self.db.begin_read().map_err(map_err)?;
            let table = rtxn.open_table(WORKERS).map_err(map_err)?;
            table.len().map_err(map_err)
        })();
        res.unwrap_or(0) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p2p_proto::{JobId, QueryHash, Verdict};

    fn cfgs() -> (TrustConfig, LimitsConfig) {
        (TrustConfig::default(), LimitsConfig::default())
    }

    fn receipt(worker: &str, verdict: Verdict, ts: u64) -> Receipt {
        Receipt {
            job_id: JobId::new(),
            worker_id: NodeId(worker.into()),
            requester_id: NodeId("b3:req".into()),
            query_hash: QueryHash::compute("SELECT 1", "t"),
            result_hash: "h".into(),
            verdict,
            latency_ms: 1,
            ts,
            requester_pubkey: String::new(),
            sig: String::new(),
        }
    }

    fn temp_path() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("p2p-trust-test-{}.redb", uuid_like()));
        p
    }

    fn uuid_like() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{n}-{:p}", &n as *const _)
            .replace("0x", "")
            .replace(' ', "")
    }

    #[test]
    fn persists_across_reopen() {
        let path = temp_path();
        let (t, l) = cfgs();
        let w = NodeId("b3:w".into());
        {
            let store = RedbTrustStore::open(&path, &t, &l).unwrap();
            store.record(&receipt("b3:w", Verdict::Correct, 100));
            store.record(&receipt("b3:w", Verdict::Correct, 100));
            store.record(&receipt("b3:w", Verdict::Incorrect, 100));
            store.add_vouch(&w, 0.3);
            store.penalize(&w, 0.25);
            assert!((store.reputation(&w, 100).unwrap() - 2.0 / 3.0).abs() < 1e-9);
        }
        // Reopen: state must survive the "restart".
        let store = RedbTrustStore::open(&path, &t, &l).unwrap();
        assert_eq!(store.observation_count(&w), 3);
        assert!((store.reputation(&w, 100).unwrap() - 2.0 / 3.0).abs() < 1e-9);
        assert!((store.voucher_trust(&w) - 0.3).abs() < 1e-9);
        assert!((store.penalty(&w) - 0.25).abs() < 1e-9);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn observation_history_is_bounded() {
        let path = temp_path();
        let t = TrustConfig::default();
        let l = LimitsConfig {
            receipt_cache_per_worker: 4,
            ..LimitsConfig::default()
        };
        let store = RedbTrustStore::open(&path, &t, &l).unwrap();
        for _ in 0..100 {
            store.record(&receipt("b3:w", Verdict::Correct, 1));
        }
        assert_eq!(store.observation_count(&NodeId("b3:w".into())), 4);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn worker_count_is_bounded_with_eviction() {
        let path = temp_path();
        let t = TrustConfig::default();
        let l = LimitsConfig {
            trust_store_capacity: 3,
            ..LimitsConfig::default()
        };
        let store = RedbTrustStore::open(&path, &t, &l).unwrap();
        for i in 0..10 {
            store.record(&receipt(&format!("b3:w{i}"), Verdict::Correct, 1));
        }
        assert_eq!(store.tracked_workers(), 3);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn unknown_worker_has_no_reputation() {
        let path = temp_path();
        let (t, l) = cfgs();
        let store = RedbTrustStore::open(&path, &t, &l).unwrap();
        assert!(store.reputation(&NodeId("b3:nope".into()), 0).is_none());
        std::fs::remove_file(&path).ok();
    }
}
