//! Integration test: the persistent (redb) trust store, opened from a
//! configured `[trust].store_path`, survives a simulated node restart and is
//! usable through the same `dyn TrustStore` seam the coordinator consumes.

use std::sync::Arc;

use p2p_config::GridConfig;
use p2p_proto::{JobId, NodeId, QueryHash, Receipt, Verdict};
use p2p_trust::{RedbTrustStore, TrustStore};

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

#[test]
fn persistent_trust_store_survives_restart_via_config_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trust.redb");

    // A config that selects the persistent store via its path knob.
    let mut cfg = GridConfig::default();
    cfg.trust.store_path = Some(path.to_string_lossy().into_owned());
    cfg.validate().unwrap();
    let store_path = cfg.trust.store_path.clone().unwrap();

    let worker = NodeId("b3:worker".into());

    // First "boot": record reputation through the trait object.
    {
        let store: Arc<dyn TrustStore> =
            Arc::new(RedbTrustStore::open(&store_path, &cfg.trust, &cfg.limits).unwrap());
        store.record(&receipt("b3:worker", Verdict::Correct, 1_000));
        store.record(&receipt("b3:worker", Verdict::Correct, 1_000));
        store.record(&receipt("b3:worker", Verdict::Incorrect, 1_000));
        store.penalize(&worker, 0.5);
        assert!((store.reputation(&worker, 1_000).unwrap() - 2.0 / 3.0).abs() < 1e-9);
    }

    // Second "boot": a brand-new store handle at the same path sees the history.
    {
        let store: Arc<dyn TrustStore> =
            Arc::new(RedbTrustStore::open(&store_path, &cfg.trust, &cfg.limits).unwrap());
        assert_eq!(store.observation_count(&worker), 3);
        assert!((store.reputation(&worker, 1_000).unwrap() - 2.0 / 3.0).abs() < 1e-9);
        assert!((store.penalty(&worker) - 0.5).abs() < 1e-9);
        assert_eq!(store.tracked_workers(), 1);
    }
}
