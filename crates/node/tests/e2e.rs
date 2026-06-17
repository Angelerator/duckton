//! End-to-end integration tests over real loopback QUIC with multiple in-process
//! nodes. Covers Phase 0 (one query end-to-end), Phase 1 (hedged racing + loser
//! cancellation + admission), and Phase 2 (quorum, cheater detection, receipts).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use p2p_config::{GridConfig, IdentityConfig, PinningMode, QueryOverrides};
use p2p_node::{
    estimate_working_set, AdmissionController, Candidate, Coordinator, CoordinatorError,
    InputObservation, InputReader, InputResolveError, InputResolver, MockEngine, QueryEngine,
    QueryShape, ScanEstimate, StaticDiscovery, Worker, WorkerParams,
};
use p2p_node::{IdentitySigner, MembershipTable};
use p2p_proto::AttestationLevel;
use p2p_proto::{Attestation, InputSnapshot, NodeId, ObjectVersion, PinnedObject, Verdict};
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
        enabled: true,
        networks: vec!["default".into()],
        groups: vec![],
        region: None,
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

// ===========================================================================
// Deterministic-input verification (source-data-drift vs quorum).
//
// These exercise the full P0→P3 fix: pinning a versioned input snapshot, the
// fingerprint-aware quorum, and the three distinct outcomes — benign input
// drift (re-dispatch, NO penalty), a genuine fault on the SAME inputs (still
// Incorrect + penalized), and an unfetchable pin (Infeasible, NO penalty) —
// plus wire back-compat for peers that report no fingerprint.
// ===========================================================================

/// A resolver that always pins the same fixed snapshot (so the test controls the
/// coordinator's pinned fingerprint without a live object store).
struct FixedResolver(InputSnapshot);

#[async_trait]
impl InputResolver for FixedResolver {
    async fn resolve(&self, _sql: &str) -> Result<Option<InputSnapshot>, InputResolveError> {
        Ok(Some(self.0.clone()))
    }
}

/// A worker input reader that reports a DIFFERENT fingerprint on its first read
/// (simulating a node that read newer source bytes), then honors the pin on
/// every subsequent read (the source stabilized) — used to drive a benign
/// drift-then-success re-dispatch.
struct DriftThenHonest {
    calls: AtomicUsize,
    drift_fp: String,
}

#[async_trait]
impl InputReader for DriftThenHonest {
    async fn observe(&self, snapshot: Option<&InputSnapshot>) -> InputObservation {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            InputObservation::Pinned(self.drift_fp.clone())
        } else {
            match snapshot {
                Some(s) => InputObservation::Pinned(s.fingerprint.clone()),
                None => InputObservation::Unpinned,
            }
        }
    }
}

/// A reader that always reports the pinned version as unfetchable (the pinned
/// object/version changed or is gone) → the worker fails the attempt as an input
/// fault (`Infeasible`), never a wrong result.
struct UnavailableReader;

#[async_trait]
impl InputReader for UnavailableReader {
    async fn observe(&self, _snapshot: Option<&InputSnapshot>) -> InputObservation {
        InputObservation::Unavailable("pinned object generation no longer exists".into())
    }
}

/// Spawn a worker with a custom [`InputReader`] (otherwise identical to
/// [`spawn_worker`]).
async fn spawn_worker_with_reader(
    engine: Arc<dyn QueryEngine>,
    reader: Arc<dyn InputReader>,
) -> WorkerHandle {
    let net = GridConfig::default().network;
    let transport =
        Arc::new(QuicTransport::bind(&net, &idcfg(), NodeIdentity::generate().unwrap()).unwrap());
    let admission = AdmissionController::new(&GridConfig::default().budget);
    let params = WorkerParams::from_config(&GridConfig::default());
    let node_id = transport.local_node_id().clone();
    let addr = transport.local_addr().unwrap();
    let worker = Worker::new(
        transport.clone(),
        engine,
        admission,
        Attestation::stub_l0(),
        params,
    )
    .with_input_reader(reader);
    let task = worker.spawn();
    WorkerHandle {
        node_id,
        addr,
        _transport: transport,
        _task: task,
    }
}

/// A coordinator wired with an input resolver (deterministic-input verification).
async fn make_coordinator_pinned(
    workers: &[&WorkerHandle],
    cfg: Arc<GridConfig>,
    store: Arc<dyn TrustStore>,
    resolver: Arc<dyn InputResolver>,
) -> Coordinator {
    make_coordinator(workers, cfg, store)
        .await
        .with_input_resolver(resolver)
}

/// A fixed, fully-concrete S3 snapshot for tests.
fn test_snapshot() -> InputSnapshot {
    InputSnapshot::from_objects(vec![PinnedObject {
        uri: "s3://bucket/events/data.parquet".into(),
        provider: "s3".into(),
        version: ObjectVersion::S3 {
            version_id: Some("v-pinned-001".into()),
            etag: Some("etag-abc".into()),
            size: 4096,
        },
    }])
}

// ---------------------------------------------------------------------------
// Drift: an honest minority that read a DIFFERENT input snapshot on a quorum
// job is NOT penalized — the attempt is benignly re-dispatched, and once the
// source stabilizes the job completes (no commitment failure, no slash).
// ---------------------------------------------------------------------------
#[tokio::test]
async fn input_drift_minority_is_not_penalized_and_redispatches() {
    let snap = test_snapshot();
    let pinned_fp = snap.fingerprint.clone();
    // F1 != pinned: the drifting worker reports it on its FIRST read only.
    let drift_fp = format!("{pinned_fp}-NEWER");

    let honest1 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let honest2 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let drifter = spawn_worker_with_reader(
        Arc::new(MockEngine::deterministic()),
        Arc::new(DriftThenHonest {
            calls: AtomicUsize::new(0),
            drift_fp,
        }),
    )
    .await;

    // replicas=3, quorum=2; allow a couple of re-dispatches.
    let mut c = (*test_config(3, 2)).clone();
    c.scheduler.max_retries = 3;
    c.scheduler.backoff_initial_ms = 1;
    c.scheduler.backoff_max_ms = 2;
    c.validate().unwrap();
    let cfg = Arc::new(c);

    let st = store();
    let coord = make_coordinator_pinned(
        &[&honest1, &honest2, &drifter],
        cfg,
        st.clone(),
        Arc::new(FixedResolver(snap)),
    )
    .await;

    // The job reaches quorum among the on-pinned replicas; the first attempt sees
    // the drifter on a different fingerprint → benign re-dispatch; the retry sees
    // it honor the pin → the job completes.
    let outcome = coord
        .run_query("SELECT * FROM read_parquet('s3://bucket/events/data.parquet')", Default::default())
        .await
        .expect("benign input drift must re-dispatch and then succeed");

    assert!(outcome.verified, "job verifies once inputs converge");
    // The previously-drifting honest worker is NEVER penalized for reading newer
    // bytes, and never recorded as a broken commitment / slash.
    assert_eq!(
        st.penalty(&drifter.node_id),
        0.0,
        "an honest minority that read drifted inputs must NOT be penalized"
    );
    assert_eq!(st.penalty(&honest1.node_id), 0.0);
    assert_eq!(st.penalty(&honest2.node_id), 0.0);
    // No participant carries an Incorrect verdict in the winning attempt.
    assert!(
        outcome.receipts.iter().all(|r| r.verdict != Verdict::Incorrect),
        "drift must never produce an Incorrect verdict"
    );
    // The verified answer is bound to the pinned input fingerprint.
    let correct = outcome
        .receipts
        .iter()
        .find(|r| r.verdict == Verdict::Correct)
        .expect("a correct receipt exists");
    assert_eq!(correct.input_fingerprint, pinned_fp);
}

// ---------------------------------------------------------------------------
// Cheater (fingerprint-aware): a worker that read the SAME pinned inputs but
// returned a WRONG result is still Incorrect + penalized — pinning must not give
// a genuine fault a free pass.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn cheater_on_same_inputs_is_still_penalized_when_pinned() {
    let snap = test_snapshot();
    let honest1 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let honest2 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    // Default reader ⇒ echoes the pinned fingerprint (same inputs), but the
    // engine perturbs the result ⇒ a genuine wrong answer on identical inputs.
    let cheater = spawn_worker(Arc::new(MockEngine::deterministic().cheating())).await;

    let cfg = test_config(3, 2);
    let st = store();
    let coord = make_coordinator_pinned(
        &[&honest1, &honest2, &cheater],
        cfg,
        st.clone(),
        Arc::new(FixedResolver(snap.clone())),
    )
    .await;

    let outcome = coord
        .run_query("SELECT * FROM read_parquet('s3://bucket/events/data.parquet')", Default::default())
        .await
        .unwrap();

    assert!(outcome.verified);
    assert_eq!(outcome.agreement, 2, "two honest workers agree on pinned inputs");
    let cheater_receipt = outcome
        .receipts
        .iter()
        .find(|r| r.worker_id == cheater.node_id)
        .expect("cheater participated");
    assert_eq!(
        cheater_receipt.verdict,
        Verdict::Incorrect,
        "a wrong result on PROVABLY identical inputs is still a fault"
    );
    assert!(
        st.penalty(&cheater.node_id) > 0.0,
        "the cheater is still penalized (fingerprint-aware quorum)"
    );
    // The cheater committed the SAME input fingerprint as the honest majority.
    assert_eq!(cheater_receipt.input_fingerprint, snap.fingerprint);
}

// ---------------------------------------------------------------------------
// Input unavailable: every selected worker cannot fetch the pinned version →
// the query is Infeasible (a job/input fault), with NO provider penalty.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn unfetchable_pinned_input_is_infeasible_without_penalty() {
    let snap = test_snapshot();
    let mk = || {
        spawn_worker_with_reader(
            Arc::new(MockEngine::deterministic()),
            Arc::new(UnavailableReader),
        )
    };
    let a = mk().await;
    let b = mk().await;
    let cc = mk().await;

    let mut c = (*test_config(3, 2)).clone();
    c.scheduler.max_retries = 1;
    c.validate().unwrap();
    let cfg = Arc::new(c);

    let st = store();
    let coord = make_coordinator_pinned(
        &[&a, &b, &cc],
        cfg,
        st.clone(),
        Arc::new(FixedResolver(snap)),
    )
    .await;

    let err = coord
        .run_query("SELECT * FROM read_parquet('s3://bucket/events/data.parquet')", Default::default())
        .await
        .unwrap_err();
    assert!(
        matches!(err, CoordinatorError::Infeasible { .. }),
        "an unfetchable pinned input is a job fault (Infeasible), got {err:?}"
    );
    // No provider is penalized for an input that became unavailable.
    assert_eq!(st.penalty(&a.node_id), 0.0);
    assert_eq!(st.penalty(&b.node_id), 0.0);
    assert_eq!(st.penalty(&cc.node_id), 0.0);
}

// ---------------------------------------------------------------------------
// Wire back-compat: with NO resolver wired (the default), workers that report no
// input fingerprint behave exactly as before — quorum is reached, the genuine
// cheater is caught, and nothing crashes on the (serde-default) empty field.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn unpinned_jobs_are_unchanged_and_back_compatible() {
    let honest1 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let honest2 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let cheater = spawn_worker(Arc::new(MockEngine::deterministic().cheating())).await;

    let cfg = test_config(3, 2);
    let st = store();
    // NO input resolver wired ⇒ pinned fingerprint is None; commits carry the
    // serde-default empty fingerprint.
    let coord = make_coordinator(&[&honest1, &honest2, &cheater], cfg, st.clone()).await;

    let outcome = coord.run_query("SELECT 7", Default::default()).await.unwrap();
    assert!(outcome.verified);
    let cheater_receipt = outcome
        .receipts
        .iter()
        .find(|r| r.worker_id == cheater.node_id)
        .expect("cheater participated");
    assert_eq!(cheater_receipt.verdict, Verdict::Incorrect);
    assert!(st.penalty(&cheater.node_id) > 0.0);
    // The empty (unknown) fingerprint is carried through receipts without issue.
    assert!(outcome.receipts.iter().all(|r| r.input_fingerprint.is_empty()));
}

// ---------------------------------------------------------------------------
// Per-job perf measurement: the winner's receipt records the estimator's
// scanned-bytes ESTIMATE (no longer hardcoded 0), which folds into both the
// proven-capability aggregate AND the rolling perf aggregate exposed for a later
// prioritization worker via `perf_sample`.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn observed_input_bytes_populated_from_estimate_and_feeds_perf() {
    let w = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let cfg = test_config(1, 1);
    let st = store();
    // No local execution wired ⇒ `run_query_planned` skips the local path and
    // dispatches to the grid, threading the estimate through to the receipt.
    let coord = make_coordinator(&[&w], cfg, st.clone()).await;

    let scan = ScanEstimate {
        scanned_uncompressed_bytes: 4_000_000,
        total_rows: 10_000,
        estimated_output_rows: 10_000,
        avg_row_width_bytes: 400,
        units_total: 1,
        units_scanned: 1,
        projected_columns: 2,
    };
    let estimate = estimate_working_set(&scan, &QueryShape::streaming(), &Default::default());

    let outcome = coord
        .run_query_planned("SELECT 1", QueryOverrides::default(), Some(estimate))
        .await
        .unwrap();
    assert!(!outcome.executed_locally, "no local exec wired ⇒ grid dispatch");

    let winner = outcome.winner.clone().expect("a winner");
    // The winner's Correct receipt carries the ESTIMATED scanned bytes, not 0.
    let wr = outcome
        .receipts
        .iter()
        .find(|r| r.worker_id == winner && r.verdict == Verdict::Correct)
        .expect("winner receipt");
    assert_eq!(wr.observed_input_bytes, 4_000_000);

    // It folded into the rolling perf aggregate on the Correct receipt.
    let perf = st.perf_aggregate(&winner).expect("perf recorded on Correct");
    assert_eq!(perf.obs_count, 1);
    assert!((perf.ewma_bytes - 4_000_000.0).abs() < 1e-6);

    // And it folded into the proven-capability size aggregate.
    let cap = st.proven_capability(&winner).expect("capability recorded");
    assert_eq!(cap.max_input_bytes, 4_000_000);

    // `perf_sample` exposes it for the (future) prioritization/pricing worker.
    let sample = coord.perf_sample(&winner).expect("perf_sample available");
    assert_eq!(sample.bytes_verified, 4_000_000);

    // A plain `run_query` (no estimate) keeps the "unknown" (0) semantics.
    let plain = coord
        .run_query("SELECT 2", QueryOverrides::default())
        .await
        .unwrap();
    let pr = plain
        .receipts
        .iter()
        .find(|r| r.verdict == Verdict::Correct)
        .expect("a correct receipt");
    assert_eq!(pr.observed_input_bytes, 0, "no estimate ⇒ unknown (0)");
}
