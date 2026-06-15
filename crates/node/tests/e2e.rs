//! End-to-end integration tests over real loopback QUIC with multiple in-process
//! nodes. Covers Phase 0 (one query end-to-end), Phase 1 (hedged racing + loser
//! cancellation + admission), and Phase 2 (quorum, cheater detection, receipts).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use p2p_config::{GridConfig, IdentityConfig, PinningMode};
use p2p_node::{
    AdmissionController, Candidate, Coordinator, CoordinatorError, MockEngine, QueryEngine,
    StaticDiscovery, Worker, WorkerParams,
};
use p2p_node::{IdentitySigner, MembershipTable};
use p2p_proto::AttestationLevel;
use p2p_proto::{Attestation, NodeId, Verdict};
use p2p_transport::{NodeIdentity, QuicTransport, Transport};
use p2p_trust::sybil::pow_epoch;
use p2p_trust::{
    mint_pow, now_ts, sign_capability_ad, CapabilityDraft, InMemoryTrustStore, TrustStore,
};

/// Keep-alive handle for a spawned worker (dropping it tears the worker down).
struct WorkerHandle {
    node_id: NodeId,
    addr: SocketAddr,
    _transport: Arc<QuicTransport>,
    _task: tokio::task::JoinHandle<()>,
}

fn idcfg() -> IdentityConfig {
    IdentityConfig {
        key_path: None,
        pinning_mode: PinningMode::Tofu,
        allowlist: vec![],
    }
}

async fn spawn_worker(engine: Arc<dyn QueryEngine>) -> WorkerHandle {
    let net = GridConfig::default().network;
    let transport =
        Arc::new(QuicTransport::bind(&net, &idcfg(), NodeIdentity::generate().unwrap()).unwrap());
    let budget = GridConfig::default().budget;
    let admission = AdmissionController::new(&budget);
    let params = WorkerParams::from_config(&GridConfig::default());
    let node_id = transport.local_node_id().clone();
    let addr = transport.local_addr().unwrap();
    let worker = Worker::new(
        transport.clone(),
        engine,
        admission,
        Attestation::stub_l0(),
        params,
    );
    let task = worker.spawn();
    WorkerHandle {
        node_id,
        addr,
        _transport: transport,
        _task: task,
    }
}

fn test_config(replicas: usize, quorum: usize) -> Arc<GridConfig> {
    let mut c = GridConfig::default();
    c.scheduler.replicas = replicas;
    c.scheduler.quorum = quorum;
    c.scheduler.offer_timeout_ms = 2_000;
    c.scheduler.dispatch_timeout_ms = 5_000;
    // Fresh test workers have no reputation; relax the trust gate so they are
    // selectable (the default policy of 0.7 intentionally blocks brand-new nodes).
    c.trust.min_trust = 0.0;
    c.discovery.candidate_sample_size = 16;
    c.validate().unwrap();
    Arc::new(c)
}

async fn make_coordinator(
    workers: &[&WorkerHandle],
    cfg: Arc<GridConfig>,
    store: Arc<dyn TrustStore>,
) -> Coordinator {
    let net = GridConfig::default().network;
    let req_transport =
        Arc::new(QuicTransport::bind(&net, &idcfg(), NodeIdentity::generate().unwrap()).unwrap());
    let candidates: Vec<Candidate> = workers
        .iter()
        .map(|w| Candidate::new(Some(w.node_id.clone()), w.addr))
        .collect();
    let discovery = Arc::new(StaticDiscovery::new(
        candidates,
        cfg.discovery.candidate_sample_size,
    ));
    Coordinator::new(req_transport, discovery, store, cfg, "mock-1")
}

fn store() -> Arc<InMemoryTrustStore> {
    Arc::new(InMemoryTrustStore::new(
        &GridConfig::default().trust,
        &GridConfig::default().limits,
    ))
}

// ---------------------------------------------------------------------------
// Phase 0 — one query end-to-end, result streamed back.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn phase0_single_worker_end_to_end() {
    let w = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let cfg = test_config(1, 1);
    let st = store();
    let coord = make_coordinator(&[&w], cfg, st.clone()).await;

    let outcome = coord
        .run_query(
            "SELECT region, count(*) FROM events GROUP BY region",
            Default::default(),
        )
        .await
        .expect("query should succeed");

    assert!(outcome.verified);
    assert_eq!(outcome.winner.as_ref(), Some(&w.node_id));
    assert_eq!(outcome.result.row_count(), 3); // mock derives 3 rows
    assert_eq!(outcome.receipts.len(), 1);
    assert_eq!(outcome.receipts[0].verdict, Verdict::Correct);
}

// ---------------------------------------------------------------------------
// Phase 1 — hedged race across k workers; fastest agreeing wins, losers RESET.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn phase1_hedged_race_fastest_agreeing_wins() {
    let fast1 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let fast2 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let slow = spawn_worker(Arc::new(
        MockEngine::deterministic().with_delay(Duration::from_millis(400)),
    ))
    .await;

    let cfg = test_config(3, 2);
    let st = store();
    let coord = make_coordinator(&[&fast1, &fast2, &slow], cfg, st.clone()).await;

    let outcome = coord
        .run_query("SELECT 42", Default::default())
        .await
        .unwrap();

    assert!(outcome.verified);
    assert_eq!(outcome.participants.len(), 3);
    // The slow worker must not win the race.
    assert_ne!(outcome.winner.as_ref(), Some(&slow.node_id));
    // All honest workers agree → quorum of 3.
    assert!(outcome.agreement >= 2);
    assert!(outcome
        .receipts
        .iter()
        .all(|r| r.verdict == Verdict::Correct));
}

// ---------------------------------------------------------------------------
// Phase 2 — quorum tolerates a cheater; cheater earns an Incorrect receipt
// and a reputation penalty.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn phase2_quorum_detects_and_penalizes_cheater() {
    let honest1 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let honest2 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let cheater = spawn_worker(Arc::new(MockEngine::deterministic().cheating())).await;

    let cfg = test_config(3, 2);
    let st = store();
    let st_dyn: Arc<dyn TrustStore> = st.clone();
    let coord = make_coordinator(&[&honest1, &honest2, &cheater], cfg, st_dyn).await;

    let outcome = coord
        .run_query("SELECT 7", Default::default())
        .await
        .unwrap();

    assert!(outcome.verified);
    assert_eq!(outcome.agreement, 2, "two honest workers agree");
    // Winner is one of the honest workers.
    assert!(
        outcome.winner.as_ref() == Some(&honest1.node_id)
            || outcome.winner.as_ref() == Some(&honest2.node_id)
    );

    // The cheater got an Incorrect receipt...
    let cheater_receipt = outcome
        .receipts
        .iter()
        .find(|r| r.worker_id == cheater.node_id)
        .expect("cheater participated");
    assert_eq!(cheater_receipt.verdict, Verdict::Incorrect);
    // ...and a reputation penalty was recorded.
    assert!(st.penalty(&cheater.node_id) > 0.0);
    // Cheater's reputation is now below the honest workers'.
    let now = p2p_trust::now_ts();
    let cheater_rep = st.reputation(&cheater.node_id, now).unwrap();
    let honest_rep = st.reputation(&honest1.node_id, now).unwrap();
    assert!(cheater_rep < honest_rep);
}

// ---------------------------------------------------------------------------
// Phase 2 — quorum fails when no q workers agree.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn phase2_quorum_fails_when_all_disagree() {
    // Three workers, each with a distinct fixture → three different hashes.
    let mk = |sql: &str, val: i64| {
        let mut f = HashMap::new();
        f.insert(
            sql.to_string(),
            p2p_proto::ResultSet::new(vec!["v".into()], vec![vec![p2p_proto::Value::Int(val)]]),
        );
        Arc::new(MockEngine::with_fixtures(f)) as Arc<dyn QueryEngine>
    };
    let a = spawn_worker(mk("SELECT x", 1)).await;
    let b = spawn_worker(mk("SELECT x", 2)).await;
    let c = spawn_worker(mk("SELECT x", 3)).await;

    let cfg = test_config(3, 2);
    let st = store();
    let coord = make_coordinator(&[&a, &b, &c], cfg, st.clone()).await;

    let err = coord
        .run_query("SELECT x", Default::default())
        .await
        .unwrap_err();
    assert!(
        matches!(err, CoordinatorError::QuorumFailed { .. }),
        "got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Phase 2 — quorum SPLIT (equivocation): two distinct hashes EACH reach quorum.
// `evaluate_quorum` flags this as a split (no safe winner); the coordinator must
// treat it as a disagreement (no silent side-pick) rather than completing.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn quorum_split_equivocation_is_surfaced_not_resolved() {
    let mk = |val: i64| {
        let mut f = HashMap::new();
        f.insert(
            "SELECT split".to_string(),
            p2p_proto::ResultSet::new(vec!["v".into()], vec![vec![p2p_proto::Value::Int(val)]]),
        );
        Arc::new(MockEngine::with_fixtures(f)) as Arc<dyn QueryEngine>
    };
    // Two workers return hash A, two return hash B → with quorum=2 BOTH A and B
    // reach quorum: an equivocation/split.
    let a1 = spawn_worker(mk(1)).await;
    let a2 = spawn_worker(mk(1)).await;
    let b1 = spawn_worker(mk(2)).await;
    let b2 = spawn_worker(mk(2)).await;

    // replicas=4 (dispatch all), quorum=2. max_retries=1 so a single split does
    // not loop (a split is terminal anyway).
    let mut c = (*test_config(4, 2)).clone();
    c.scheduler.max_retries = 1;
    c.validate().unwrap();
    let cfg = Arc::new(c);
    let st = store();
    let coord = make_coordinator(&[&a1, &a2, &b1, &b2], cfg, st.clone()).await;

    let err = coord
        .run_query("SELECT split", Default::default())
        .await
        .unwrap_err();
    // A split surfaces as QuorumFailed with BOTH hashes at the quorum count (2/2)
    // — the signature of the equivocation branch (a generic shortfall would carry
    // a lower agreement). The coordinator never silently picked A or B.
    match err {
        CoordinatorError::QuorumFailed { agreement, quorum } => {
            assert_eq!(quorum, 2, "got {err:?}");
            assert_eq!(
                agreement, 2,
                "split: both hashes reached quorum; got {err:?}"
            );
        }
        other => panic!("expected QuorumFailed (split), got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Phase 2 — receipts build reputation across repeated jobs.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn phase2_receipts_accumulate_reputation() {
    let w1 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let w2 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let cfg = test_config(2, 2);
    let st = store();
    let coord = make_coordinator(&[&w1, &w2], cfg, st.clone()).await;

    for i in 0..3 {
        coord
            .run_query(&format!("SELECT {i}"), Default::default())
            .await
            .unwrap();
    }
    let now = p2p_trust::now_ts();
    // Both honest workers should have accrued correct observations → rep 1.0.
    assert!(st.observation_count(&w1.node_id) >= 1);
    assert_eq!(st.reputation(&w1.node_id, now), Some(1.0));
    assert_eq!(st.reputation(&w2.node_id, now), Some(1.0));
}

// ---------------------------------------------------------------------------
// Phase 3 — discovery via signed capability ads + bounded membership sampling,
// driving a real hedged query end-to-end.
// ---------------------------------------------------------------------------
fn build_ad(w: &WorkerHandle) -> p2p_proto::CapabilityAd {
    let id = w._transport.identity();
    let pk = id.public_key_bytes();
    let ts = now_ts();
    let draft = CapabilityDraft {
        addr: w.addr.to_string(),
        free_mem_bytes: 1 << 30,
        free_threads: 4,
        max_jobs: 3,
        attestation_level: AttestationLevel::L0,
        price: 0,
        recent_receipts_root: None,
        pow: mint_pow(&pk, pow_epoch(ts), 8, 1_000_000).unwrap(),
        ts,
    };
    sign_capability_ad(draft, &IdentitySigner(id))
}

#[tokio::test]
async fn phase3_discovery_via_capability_ads() {
    let w1 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let w2 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let w3 = spawn_worker(Arc::new(MockEngine::deterministic())).await;

    // Workers publish signed capability ads into the requester's bounded
    // membership view (PoW-gated, signature-verified).
    let membership = Arc::new(MembershipTable::new(1000, 8, 3600));
    assert!(membership.ingest(build_ad(&w1)));
    assert!(membership.ingest(build_ad(&w2)));
    assert!(membership.ingest(build_ad(&w3)));

    let cfg = test_config(3, 2);
    let net = GridConfig::default().network;
    let req_transport =
        Arc::new(QuicTransport::bind(&net, &idcfg(), NodeIdentity::generate().unwrap()).unwrap());
    let st = store();
    let coord = Coordinator::new(req_transport, membership, st, cfg, "mock-1");

    let outcome = coord
        .run_query("SELECT 1", Default::default())
        .await
        .unwrap();
    assert!(outcome.verified);
    assert_eq!(outcome.participants.len(), 3);
}

// ---------------------------------------------------------------------------
// Phase 1 — not enough trustworthy workers → InsufficientWorkers.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn insufficient_workers_when_trust_gate_excludes_all() {
    let w = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let mut c = (*test_config(1, 1)).clone();
    c.trust.min_trust = 0.99; // fresh worker can't clear this
    let cfg = Arc::new(c);
    let st = store();
    let coord = make_coordinator(&[&w], cfg, st.clone()).await;

    let err = coord
        .run_query("SELECT 1", Default::default())
        .await
        .unwrap_err();
    assert!(
        matches!(err, CoordinatorError::InsufficientWorkers { .. }),
        "got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Loser cancellation timing — a losing worker that is still computing when the
// race is decided must be aborted PROMPTLY (its stream reset BEFORE the winner's
// result is downloaded), not left to run for the whole winner transfer. We
// verify the loser's engine never reaches completion even after we wait well
// past its (long) execution delay.
// ---------------------------------------------------------------------------
mod cancel_timing {
    use super::*;
    use async_trait::async_trait;
    use p2p_node::{EngineError, ExecLease, QueryEngine};
    use p2p_proto::{ResultSet, Value};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// An engine that sleeps `delay` and only THEN records a completion. If its
    /// execution future is dropped (the worker aborted a cancelled job) the
    /// counter is never incremented — exactly what we assert for a loser.
    struct SlowCountingEngine {
        delay: Duration,
        completed: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl QueryEngine for SlowCountingEngine {
        async fn execute(&self, _sql: &str, _lease: ExecLease) -> Result<ResultSet, EngineError> {
            tokio::time::sleep(self.delay).await;
            // Reached only if NOT cancelled mid-execution.
            self.completed.fetch_add(1, Ordering::SeqCst);
            Ok(ResultSet::new(
                vec!["k".into(), "v".into()],
                (0..3u8)
                    .map(|i| vec![Value::Int(i as i64), Value::Int(7)])
                    .collect(),
            ))
        }
        fn version(&self) -> String {
            // Same version as the fast workers so hashes can agree on identical rows.
            "mock-1".to_string()
        }
    }

    #[tokio::test]
    async fn losing_worker_is_cancelled_before_winner_download() {
        // Two fast workers (instant) form the quorum and provide the winner; the
        // result they return is identical to what the slow engine WOULD return
        // (same rows), so all three would agree — the slow one only loses on speed.
        let fast_rows = ResultSet::new(
            vec!["k".into(), "v".into()],
            (0..3u8)
                .map(|i| vec![Value::Int(i as i64), Value::Int(7)])
                .collect(),
        );
        let mut fix = HashMap::new();
        fix.insert("SELECT loser_cancel".to_string(), fast_rows);
        let fast1 = spawn_worker(Arc::new(MockEngine::with_fixtures(fix.clone()))).await;
        let fast2 = spawn_worker(Arc::new(MockEngine::with_fixtures(fix))).await;

        let completed = Arc::new(AtomicUsize::new(0));
        // The loser takes far longer to EXECUTE than the requester's per-read
        // dispatch deadline: the coordinator gives up waiting on it (treats it as
        // silent), reaches quorum from the two fast workers, and resets the slow
        // worker's stream WHILE it is still executing. The slow worker must abort
        // promptly (cancel-watch) rather than run its full delay to completion.
        let loser_delay = Duration::from_millis(2000);
        let slow = spawn_worker(Arc::new(SlowCountingEngine {
            delay: loser_delay,
            completed: completed.clone(),
        }))
        .await;

        // replicas=3 (dispatch to all), quorum=2 (the two fast workers).
        let mut c = (*test_config(3, 2)).clone();
        // The host execution deadline is generous (so the slow worker is stopped
        // by the CANCEL, not by its own job timeout — we are testing cancellation).
        c.worker.job_timeout_ms = 60_000;
        // DISABLE host progress streaming (interval = 0): the loser must be aborted
        // by the cancel/reset on the dispatch stream itself, not because a periodic
        // progress write happened to fail. This exercises the worker-side
        // cancel-watch race directly.
        c.worker.progress_interval_ms = 0;
        // Requester side: disable progress-based stall detection so the per-read
        // stall timeout falls back to `dispatch_timeout_ms`. With a short dispatch
        // timeout the coordinator stops waiting on the still-executing slow worker
        // quickly (treats it as silent), reaches quorum from the fast workers, and
        // resets the slow worker mid-execution.
        c.scheduler.progress_interval_ms = 0;
        c.scheduler.dispatch_timeout_ms = 500;
        c.scheduler.attempt_deadline_ms = 5_000;
        c.validate().unwrap();
        let cfg = Arc::new(c);
        let st = store();
        let coord = make_coordinator(&[&fast1, &fast2, &slow], cfg, st).await;

        let start = std::time::Instant::now();
        let outcome = coord
            .run_query("SELECT loser_cancel", Default::default())
            .await
            .unwrap();
        // The fast workers decided the race well before the loser's delay elapsed.
        assert!(outcome.verified);
        assert_ne!(
            outcome.winner.as_ref(),
            Some(&slow.node_id),
            "slow worker must not win"
        );
        assert!(
            start.elapsed() < loser_delay,
            "race should resolve (from the fast workers) before the loser would finish ({:?})",
            start.elapsed()
        );

        // Wait well past the loser's would-be completion: if it were left running
        // (not raced against the cancel/reset), the counter would tick to 1. With
        // prompt cancellation mid-execution it stays 0.
        tokio::time::sleep(loser_delay + Duration::from_millis(750)).await;
        assert_eq!(
            completed.load(Ordering::SeqCst),
            0,
            "losing worker kept computing after being cancelled (no prompt mid-execution abort)"
        );
    }
}
