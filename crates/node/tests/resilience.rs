//! Resilience / liveness layer (architecture §8/§11): host + requester
//! timeouts, streamed progress/heartbeat stall detection, phi-accrual + SWIM
//! liveness exclusion, resilient re-dispatch (unlimited-by-default retries with
//! backoff + a global retry budget + fault attribution), and the paid
//! broken-commitment fine (slash).
//!
//! Deterministic over real loopback QUIC with the mock engine and hand-rolled
//! protocol workers. NO network, NO live TON.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use p2p_config::{
    DataClassCfg, GridConfig, IdentityConfig, LivenessConfig, PaymentPref, PinningMode,
    QueryOverrides, SettlementRail,
};
use p2p_node::{
    AdmissionController, Candidate, CandidateFilter, Coordinator, CoordinatorError, Discovery,
    LivenessFilteredDiscovery, LivenessView, MockEngine, QueryEngine, StaticDiscovery, SwimVerdict,
    Worker, WorkerParams, WorkingSetEstimate,
};
use p2p_proto::{Ack, Attestation, Bid, BidDecision, NodeId, Offer, Progress, Wire};
use p2p_settlement::types::{Amount, SlashError};
use p2p_settlement::{InMemoryStakeRegistry, SlashReason, StakeRegistry};
use p2p_transport::endpoint::{read_msg, write_msg};
use p2p_transport::{NodeIdentity, QuicTransport, Transport};
use p2p_trust::{InMemoryTrustStore, TrustStore};

const TON: Amount = 1_000_000_000;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

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

fn transport() -> Arc<QuicTransport> {
    let net = GridConfig::default().network;
    Arc::new(QuicTransport::bind(&net, &idcfg(), NodeIdentity::generate().unwrap()).unwrap())
}

/// A normal worker that commits fast (the "healthy" provider).
async fn spawn_worker(engine: Arc<dyn QueryEngine>) -> WorkerHandle {
    spawn_worker_cfg(engine, GridConfig::default()).await
}

/// A worker built from an explicit config (used to disable progress + set the
/// host job_timeout for the abandon/silent scenarios).
async fn spawn_worker_cfg(engine: Arc<dyn QueryEngine>, cfg: GridConfig) -> WorkerHandle {
    let transport = transport();
    let admission = AdmissionController::new(&cfg.budget);
    let params = WorkerParams::from_config(&cfg);
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

/// A worker that ACCEPTS offers but never delivers a result: its engine hangs
/// and progress streaming + the host deadline are disabled, so the requester
/// observes pure silence (no progress, no commit) and must re-dispatch.
async fn spawn_silent_worker() -> WorkerHandle {
    let mut cfg = GridConfig::default();
    cfg.worker.progress_interval_ms = 0; // no heartbeats
    cfg.worker.job_timeout_ms = 0; // never abandon — just hang
    let engine = Arc::new(MockEngine::deterministic().with_delay(Duration::from_secs(3600)));
    spawn_worker_cfg(engine, cfg).await
}

/// A worker whose HOST execution deadline fires: it accepts, then its engine
/// runs past `job_timeout_ms` and the host ABANDONS the job (drops the stream).
async fn spawn_abandoning_worker(job_timeout_ms: u64) -> WorkerHandle {
    let mut cfg = GridConfig::default();
    cfg.worker.progress_interval_ms = 0;
    cfg.worker.job_timeout_ms = job_timeout_ms;
    let engine = Arc::new(MockEngine::deterministic().with_delay(Duration::from_secs(3600)));
    spawn_worker_cfg(engine, cfg).await
}

/// A hand-rolled worker that ACCEPTS the offer, streams `n_progress` heartbeats
/// spaced by `interval`, then STALLS (keeps the stream open but sends nothing
/// more — no commit). Exercises the requester's progress-stall detection.
async fn spawn_stalling_worker(n_progress: usize, interval: Duration) -> WorkerHandle {
    let transport = transport();
    let node_id = transport.local_node_id().clone();
    let addr = transport.local_addr().unwrap();
    let t = transport.clone();
    let nid = node_id.clone();
    let task = tokio::spawn(async move {
        while let Some(Ok(conn)) = t.accept().await {
            let nid = nid.clone();
            tokio::spawn(async move {
                loop {
                    let (mut send, mut recv) = match conn.accept_bi().await {
                        Ok(x) => x,
                        Err(_) => break,
                    };
                    let nid = nid.clone();
                    tokio::spawn(async move {
                        match read_msg(&mut recv).await {
                            Ok(Wire::Offer(o)) => {
                                let _ =
                                    write_msg(&mut send, &Wire::Bid(accept_bid(&o, &nid))).await;
                                let _ = send.finish();
                            }
                            Ok(Wire::Dispatch(d)) => {
                                for i in 0..n_progress {
                                    tokio::time::sleep(interval).await;
                                    let p = Progress {
                                        job_id: d.job_id.clone(),
                                        worker_id: nid.clone(),
                                        stage: "executing".into(),
                                        rows_processed: (i as u64 + 1) * 10,
                                        pct: 0,
                                        seq: i as u32 + 1,
                                        ts_ms: 0,
                                    };
                                    if write_msg(&mut send, &Wire::Progress(p)).await.is_err() {
                                        return;
                                    }
                                }
                                // Stall: keep the stream open, send nothing more.
                                std::future::pending::<()>().await;
                            }
                            _ => {}
                        }
                    });
                }
            });
        }
    });
    WorkerHandle {
        node_id,
        addr,
        _transport: transport,
        _task: task,
    }
}

fn accept_bid(offer: &Offer, worker_id: &NodeId) -> Bid {
    Bid {
        job_id: offer.job_id.clone(),
        worker_id: worker_id.clone(),
        decision: BidDecision::Accept,
        eta_ms: 10,
        price: 0,
        attestation: Attestation::stub_l0(),
        recent_receipts: vec![],
        free_mem_bytes: 1 << 30,
        free_threads: 4,
        region_proof: None,
        rate_per_second: 0,
        rate_per_gb: 0,
        estimated_seconds: 0,
        cap_seconds: 0,
    }
}

fn store() -> Arc<InMemoryTrustStore> {
    Arc::new(InMemoryTrustStore::new(
        &GridConfig::default().trust,
        &GridConfig::default().limits,
    ))
}

/// A fast resilience config: small stall/attempt windows + backoff so the tests
/// run in milliseconds, trust gate relaxed for fresh workers.
fn fast_cfg(replicas: usize, quorum: usize) -> GridConfig {
    let mut c = GridConfig::default();
    c.scheduler.replicas = replicas;
    c.scheduler.quorum = quorum;
    c.scheduler.offer_timeout_ms = 1_000;
    c.scheduler.dispatch_timeout_ms = 1_000;
    c.scheduler.attempt_deadline_ms = 1_000;
    c.scheduler.progress_interval_ms = 40; // stall = 40 * 3 = 120ms
    c.scheduler.progress_stall_multiplier = 3;
    c.scheduler.backoff_initial_ms = 5;
    c.scheduler.backoff_max_ms = 20;
    c.scheduler.backoff_jitter_frac = 0.0; // deterministic timing
    c.trust.min_trust = 0.0;
    c.discovery.candidate_sample_size = 64;
    c.validate().unwrap();
    c
}

fn candidates_of(workers: &[&WorkerHandle]) -> Vec<Candidate> {
    workers
        .iter()
        .map(|w| Candidate::new(Some(w.node_id.clone()), w.addr))
        .collect()
}

async fn coord_with(
    disc: Arc<dyn Discovery>,
    cfg: GridConfig,
    st: Arc<dyn TrustStore>,
) -> Coordinator {
    Coordinator::new(transport(), disc, st, Arc::new(cfg), "mock-1")
}

/// Discovery that hands out a different candidate set per call (call N returns
/// `scripts[min(N, len-1)]`), to drive deterministic re-dispatch scenarios.
struct ScriptedDiscovery {
    scripts: Vec<Vec<Candidate>>,
    call: AtomicUsize,
}
impl ScriptedDiscovery {
    fn new(scripts: Vec<Vec<Candidate>>) -> Self {
        Self {
            scripts,
            call: AtomicUsize::new(0),
        }
    }
}
#[async_trait]
impl Discovery for ScriptedDiscovery {
    async fn find_candidates(&self, _want: usize, _filter: CandidateFilter) -> Vec<Candidate> {
        let n = self.call.fetch_add(1, Ordering::SeqCst);
        let idx = n.min(self.scripts.len().saturating_sub(1));
        self.scripts.get(idx).cloned().unwrap_or_default()
    }
}

/// A stake registry that records every `slash` call (for assertions), delegating
/// stake bookkeeping to an inner in-memory registry.
struct RecordingStakeRegistry {
    inner: InMemoryStakeRegistry,
    slashes: Mutex<Vec<(NodeId, SlashReason, Amount)>>,
}
impl RecordingStakeRegistry {
    fn new(inner: InMemoryStakeRegistry) -> Self {
        Self {
            inner,
            slashes: Mutex::new(Vec::new()),
        }
    }
    fn slashes(&self) -> Vec<(NodeId, SlashReason, Amount)> {
        self.slashes.lock().unwrap().clone()
    }
}
impl StakeRegistry for RecordingStakeRegistry {
    fn stake_of(&self, node: &NodeId) -> Amount {
        self.inner.stake_of(node)
    }
    fn is_eligible(&self, node: &NodeId, class: DataClassCfg) -> bool {
        self.inner.is_eligible(node, class)
    }
    fn stake_factor(&self, node: &NodeId) -> f64 {
        self.inner.stake_factor(node)
    }
    fn slash(&self, node: &NodeId, reason: SlashReason, amount: Amount) -> Result<(), SlashError> {
        self.slashes
            .lock()
            .unwrap()
            .push((node.clone(), reason, amount));
        self.inner.slash(node, reason, amount)
    }
    fn request_unbond(&self, node: &NodeId, amount: Amount) -> Result<(), SlashError> {
        self.inner.request_unbond(node, amount)
    }
}

fn paid_cfg(mut c: GridConfig) -> GridConfig {
    c.economics.enabled = true;
    c.economics.default_payment = PaymentPref::Paid;
    c.economics.settlement = SettlementRail::Channel;
    c.economics.fee_recipient = Some(format!("0:{}", "00".repeat(32)));
    c.economics.pricing.max_bid = 100;
    c.economics.slashing.slash_failed_commitment_pct = 0.1;
    c.validate().unwrap();
    c
}

// ===========================================================================
// 1. Host job_timeout fires → re-dispatch
// ===========================================================================
#[tokio::test]
async fn host_job_timeout_abandons_and_redispatches() {
    let abandoner = spawn_abandoning_worker(60).await;
    let healthy = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    // Attempt 1 sees only the abandoner; attempt 2 sees the healthy worker.
    let disc = Arc::new(ScriptedDiscovery::new(vec![
        candidates_of(&[&abandoner]),
        candidates_of(&[&healthy]),
    ]));
    let coord = coord_with(disc, fast_cfg(1, 1), store()).await;

    let outcome = coord
        .run_query("SELECT 1", QueryOverrides::default())
        .await
        .unwrap();
    assert!(outcome.verified);
    assert_eq!(
        outcome.winner.as_ref(),
        Some(&healthy.node_id),
        "fresh healthy node must win after abandon"
    );
}

// ===========================================================================
// 2. All selected nodes silent → re-dispatch to a new set
// ===========================================================================
#[tokio::test]
async fn all_silent_redispatches_to_a_fresh_set() {
    let s1 = spawn_silent_worker().await;
    let s2 = spawn_silent_worker().await;
    let h1 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let h2 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let disc = Arc::new(ScriptedDiscovery::new(vec![
        candidates_of(&[&s1, &s2]),
        candidates_of(&[&h1, &h2]),
    ]));
    let coord = coord_with(disc, fast_cfg(2, 2), store()).await;

    let outcome = coord
        .run_query("SELECT 1", QueryOverrides::default())
        .await
        .unwrap();
    assert!(outcome.verified);
    assert!(outcome
        .participants
        .iter()
        .all(|p| *p == h1.node_id || *p == h2.node_id));
}

// ===========================================================================
// 3. Progress-stall (updates stop) detected → re-dispatch
// ===========================================================================
#[tokio::test]
async fn progress_stall_detected_redispatches() {
    // Streams 2 heartbeats (resetting the stall timer) then goes silent.
    let staller = spawn_stalling_worker(2, Duration::from_millis(30)).await;
    let healthy = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let disc = Arc::new(ScriptedDiscovery::new(vec![
        candidates_of(&[&staller]),
        candidates_of(&[&healthy]),
    ]));
    let coord = coord_with(disc, fast_cfg(1, 1), store()).await;

    let outcome = coord
        .run_query("SELECT 1", QueryOverrides::default())
        .await
        .unwrap();
    assert!(outcome.verified);
    assert_eq!(
        outcome.winner.as_ref(),
        Some(&healthy.node_id),
        "stalled node must be routed around"
    );
}

// ===========================================================================
// 4. phi-accrual marks a silent node dead and selection excludes it
// ===========================================================================
#[tokio::test]
async fn phi_convicted_node_is_excluded_from_selection() {
    let dead = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let healthy = spawn_worker(Arc::new(MockEngine::deterministic())).await;

    let view = Arc::new(LivenessView::new(LivenessConfig::default()));
    // Convict `dead` directly (SWIM confirmed dead) — equivalent to phi crossing
    // the threshold with no rescue.
    view.apply_swim(&dead.node_id, SwimVerdict::Dead);

    let inner = Arc::new(StaticDiscovery::new(candidates_of(&[&dead, &healthy]), 64));
    let disc = Arc::new(LivenessFilteredDiscovery::new(inner, Arc::clone(&view)));
    let coord = coord_with(disc, fast_cfg(1, 1), store())
        .await
        .with_liveness(view);

    // Run several times: the convicted node must never be selected/win.
    for _ in 0..4 {
        let outcome = coord
            .run_query("SELECT 1", QueryOverrides::default())
            .await
            .unwrap();
        assert_eq!(outcome.winner.as_ref(), Some(&healthy.node_id));
        assert!(outcome.participants.iter().all(|p| *p != dead.node_id));
    }
}

// ===========================================================================
// 5. Unlimited-retry loops with backoff until a (later-healthy) node succeeds
// ===========================================================================
#[tokio::test]
async fn unlimited_retry_until_a_later_healthy_node_succeeds() {
    let s1 = spawn_silent_worker().await;
    let s2 = spawn_silent_worker().await;
    let healthy = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    // Two silent attempts, then a healthy node appears.
    let disc = Arc::new(ScriptedDiscovery::new(vec![
        candidates_of(&[&s1]),
        candidates_of(&[&s2]),
        candidates_of(&[&healthy]),
    ]));
    let mut cfg = fast_cfg(1, 1);
    cfg.scheduler.max_retries = 0; // unlimited
    let coord = coord_with(disc, cfg, store()).await;

    let outcome = coord
        .run_query("SELECT 1", QueryOverrides::default())
        .await
        .unwrap();
    assert!(outcome.verified);
    assert_eq!(outcome.winner.as_ref(), Some(&healthy.node_id));
}

// ===========================================================================
// 6. Fault-attribution STOPS on a consensus-infeasible query (no endless retry)
// ===========================================================================
#[tokio::test]
async fn consensus_infeasible_query_stops_without_retry() {
    // All three selected nodes fail the SAME deterministic way (catalog error).
    let mk = || {
        Arc::new(MockEngine::failing(
            "Catalog Error: Table 'missing' does not exist",
        )) as Arc<dyn QueryEngine>
    };
    let a = spawn_worker(mk()).await;
    let b = spawn_worker(mk()).await;
    let c = spawn_worker(mk()).await;
    let disc = Arc::new(StaticDiscovery::new(candidates_of(&[&a, &b, &c]), 64));
    let coord = coord_with(disc, fast_cfg(3, 2), store()).await;

    let err = coord
        .run_query("SELECT * FROM missing", QueryOverrides::default())
        .await
        .unwrap_err();
    assert!(
        matches!(err, CoordinatorError::Infeasible { .. }),
        "got {err:?}"
    );
}

// ===========================================================================
// 7. Retry/hedge budget caps a storm
// ===========================================================================
#[tokio::test]
async fn retry_budget_caps_a_storm() {
    let s0 = spawn_silent_worker().await;
    let s1 = spawn_silent_worker().await;
    let s2 = spawn_silent_worker().await;
    let s3 = spawn_silent_worker().await;
    // A fresh silent worker every attempt → the loop would never stop on its own;
    // only the budget caps it.
    let disc = Arc::new(ScriptedDiscovery::new(vec![
        candidates_of(&[&s0]),
        candidates_of(&[&s1]),
        candidates_of(&[&s2]),
        candidates_of(&[&s3]),
    ]));
    let mut cfg = fast_cfg(1, 1);
    cfg.scheduler.max_retries = 0; // unlimited retries
    cfg.scheduler.retry_budget_max_tokens = 2.0; // but only 2 retry tokens
    cfg.scheduler.retry_budget_refill_per_sec = 0.0; // no refill during the run
    let coord = coord_with(disc, cfg, store()).await;

    let err = coord
        .run_query("SELECT 1", QueryOverrides::default())
        .await
        .unwrap_err();
    assert!(
        matches!(err, CoordinatorError::RetryBudgetExhausted { attempts } if attempts == 3),
        "got {err:?}",
    );
}

// ===========================================================================
// 8. Config precedence + per-call overrides (max_total_duration caps the loop)
// ===========================================================================
#[tokio::test]
async fn per_call_overrides_apply_and_max_total_duration_caps() {
    let s0 = spawn_silent_worker().await;
    let s1 = spawn_silent_worker().await;
    let s2 = spawn_silent_worker().await;
    let disc = Arc::new(ScriptedDiscovery::new(vec![
        candidates_of(&[&s0]),
        candidates_of(&[&s1]),
        candidates_of(&[&s2]),
    ]));
    let coord = coord_with(disc, fast_cfg(1, 1), store()).await;

    // Per-call overrides win over config: unlimited retries but a tiny wall-clock
    // cap, so the loop stops with Exhausted rather than running forever.
    let outcome = coord
        .run_query(
            "SELECT 1",
            QueryOverrides {
                max_retries: Some(0),
                max_total_duration_ms: Some(50),
                attempt_deadline_ms: Some(500),
                ..Default::default()
            },
        )
        .await;
    assert!(
        matches!(outcome, Err(CoordinatorError::Exhausted { .. })),
        "got {outcome:?}",
    );
}

// ===========================================================================
// Broken-commitment FINE (slash) — paid jobs
// ===========================================================================

/// A paid job where one accepted provider fails to deliver while another
/// delivers a valid result → the failer is FINED `FailedCommitment` (assert the
/// slash call + amount = stake * pct). The delivering winner is paid, not fined.
#[tokio::test]
async fn paid_broken_commitment_is_fined() {
    let healthy = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let failer = spawn_silent_worker().await;

    // quorum=1: the single healthy commit proves the job FEASIBLE, so the silent
    // provider's failure is a broken commitment (not a query problem).
    let cfg = paid_cfg(fast_cfg(2, 1));
    let reg = Arc::new(RecordingStakeRegistry::new(InMemoryStakeRegistry::new(
        0,
        0,
        0,
        100_000 * TON,
    )));
    reg.inner.set_stake(&healthy.node_id, 1_000 * TON);
    reg.inner.set_stake(&failer.node_id, 1_000 * TON);

    // Both available; the silent one is dispatched alongside the healthy one.
    let disc = Arc::new(StaticDiscovery::new(
        candidates_of(&[&healthy, &failer]),
        64,
    ));
    let coord = coord_with(disc, cfg, store())
        .await
        .with_stake_registry(reg.clone());

    let outcome = coord
        .run_query("SELECT 1", QueryOverrides::default())
        .await
        .unwrap();
    assert!(
        outcome.verified,
        "single commit reaches quorum=1 → feasible"
    );
    assert_eq!(outcome.winner.as_ref(), Some(&healthy.node_id));

    let slashes = reg.slashes();
    assert_eq!(
        slashes.len(),
        1,
        "exactly the one non-delivering provider is fined, got {slashes:?}"
    );
    let (node, reason, amount) = &slashes[0];
    assert_eq!(
        node, &failer.node_id,
        "the failer is fined, not the deliverer"
    );
    assert_eq!(*reason, SlashReason::FailedCommitment);
    assert_eq!(*amount, (1_000 * TON) / 10, "fine = 10% of bonded stake");
}

/// A consensus-infeasible PAID job → the query is the problem, NOT the providers
/// → NO fine for anyone.
#[tokio::test]
async fn consensus_infeasible_paid_job_fines_no_one() {
    let mk = || {
        Arc::new(MockEngine::failing(
            "Catalog Error: Table 'x' does not exist",
        )) as Arc<dyn QueryEngine>
    };
    let a = spawn_worker(mk()).await;
    let b = spawn_worker(mk()).await;
    let c = spawn_worker(mk()).await;

    let cfg = paid_cfg(fast_cfg(3, 2));
    let reg = Arc::new(RecordingStakeRegistry::new(InMemoryStakeRegistry::new(
        0,
        0,
        0,
        100_000 * TON,
    )));
    for w in [&a, &b, &c] {
        reg.inner.set_stake(&w.node_id, 1_000 * TON);
    }
    let disc = Arc::new(StaticDiscovery::new(candidates_of(&[&a, &b, &c]), 64));
    let coord = coord_with(disc, cfg, store())
        .await
        .with_stake_registry(reg.clone());

    let err = coord
        .run_query("SELECT * FROM x", QueryOverrides::default())
        .await
        .unwrap_err();
    assert!(
        matches!(err, CoordinatorError::Infeasible { .. }),
        "got {err:?}"
    );
    assert!(
        reg.slashes().is_empty(),
        "infeasible (job-fault) job fines nobody"
    );
}

/// A FREE job with a non-delivering node → NO fine (no money was asked); the
/// provider's reputation may drop, but the stake is never slashed.
#[tokio::test]
async fn free_job_non_delivering_node_is_not_fined() {
    let healthy = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let failer = spawn_silent_worker().await;

    // Default (free) economics.
    let cfg = fast_cfg(2, 1);
    assert!(!cfg.economics.enabled);
    let reg = Arc::new(RecordingStakeRegistry::new(InMemoryStakeRegistry::new(
        0,
        0,
        0,
        100_000 * TON,
    )));
    reg.inner.set_stake(&failer.node_id, 1_000 * TON); // staked, but free job

    let disc = Arc::new(StaticDiscovery::new(
        candidates_of(&[&healthy, &failer]),
        64,
    ));
    let coord = coord_with(disc, cfg, store())
        .await
        .with_stake_registry(reg.clone());

    let outcome = coord
        .run_query("SELECT 1", QueryOverrides::default())
        .await
        .unwrap();
    assert!(outcome.verified);
    assert!(
        reg.slashes().is_empty(),
        "a free job never fines a non-delivering provider"
    );
    // Reputation path still ran: the non-deliverer earned a (provider-fault) receipt.
    assert!(
        outcome
            .receipts
            .iter()
            .any(|r| r.worker_id == failer.node_id),
        "the non-deliverer should still get a (reputation) receipt on a free job",
    );
}

/// An UNSTAKED free-tier provider that fails to deliver on a paid job → no
/// stake to slash, so no fine (reputation only).
#[tokio::test]
async fn unstaked_provider_is_not_fined() {
    let healthy = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let failer = spawn_silent_worker().await;

    let cfg = paid_cfg(fast_cfg(2, 1));
    let reg = Arc::new(RecordingStakeRegistry::new(InMemoryStakeRegistry::new(
        0,
        0,
        0,
        100_000 * TON,
    )));
    reg.inner.set_stake(&healthy.node_id, 1_000 * TON);
    // `failer` intentionally has ZERO stake (free-tier provider).

    let disc = Arc::new(StaticDiscovery::new(
        candidates_of(&[&healthy, &failer]),
        64,
    ));
    let coord = coord_with(disc, cfg, store())
        .await
        .with_stake_registry(reg.clone());

    let outcome = coord
        .run_query("SELECT 1", QueryOverrides::default())
        .await
        .unwrap();
    assert!(outcome.verified);
    assert!(
        reg.slashes().is_empty(),
        "an unstaked provider has no bond to fine, got {:?}",
        reg.slashes(),
    );
}

// ===========================================================================
// Robust failover + size-based capability gate (Part A.1 / Part B.2)
// ===========================================================================

/// Accept bid advertising a chosen free-memory capacity (drives the size gate /
/// re-route capacity floor).
fn accept_bid_mem(offer: &Offer, worker_id: &NodeId, free_mem: u64) -> Bid {
    let mut b = accept_bid(offer, worker_id);
    b.free_mem_bytes = free_mem;
    b
}

/// A hand-rolled worker that ACCEPTS (advertising `free_mem`) then, on dispatch,
/// reports an OUT-OF-MEMORY failure — i.e. the job was too big for it. The
/// requester classifies this as `ResourceExceeded`.
async fn spawn_oom_worker(free_mem: u64) -> WorkerHandle {
    let transport = transport();
    let node_id = transport.local_node_id().clone();
    let addr = transport.local_addr().unwrap();
    let t = transport.clone();
    let nid = node_id.clone();
    let task = tokio::spawn(async move {
        while let Some(Ok(conn)) = t.accept().await {
            let nid = nid.clone();
            tokio::spawn(async move {
                loop {
                    let (mut send, mut recv) = match conn.accept_bi().await {
                        Ok(x) => x,
                        Err(_) => break,
                    };
                    let nid = nid.clone();
                    tokio::spawn(async move {
                        match read_msg(&mut recv).await {
                            Ok(Wire::Offer(o)) => {
                                let _ = write_msg(
                                    &mut send,
                                    &Wire::Bid(accept_bid_mem(&o, &nid, free_mem)),
                                )
                                .await;
                                let _ = send.finish();
                            }
                            Ok(Wire::Dispatch(d)) => {
                                let _ = write_msg(
                                    &mut send,
                                    &Wire::Ack(Ack {
                                        job_id: d.job_id.clone(),
                                        ok: false,
                                        detail: "Out of Memory Error: failed to allocate".into(),
                                    }),
                                )
                                .await;
                                let _ = send.finish();
                            }
                            _ => {}
                        }
                    });
                }
            });
        }
    });
    WorkerHandle {
        node_id,
        addr,
        _transport: transport,
        _task: task,
    }
}

/// A hand-rolled worker that ACCEPTS every offer (advertising ample free memory)
/// but then REJECTS every dispatch with an `Ack { ok: false, detail: "at
/// capacity" }` — exactly the admission-decline a dual-role host emits when a
/// dispatch sized at `per_job` exceeds its served ceiling. The requester
/// classifies this as a (neutral) `Inconclusive` failure and must route around
/// it, never hang.
async fn spawn_at_capacity_worker() -> WorkerHandle {
    let transport = transport();
    let node_id = transport.local_node_id().clone();
    let addr = transport.local_addr().unwrap();
    let t = transport.clone();
    let nid = node_id.clone();
    let task = tokio::spawn(async move {
        while let Some(Ok(conn)) = t.accept().await {
            let nid = nid.clone();
            tokio::spawn(async move {
                loop {
                    let (mut send, mut recv) = match conn.accept_bi().await {
                        Ok(x) => x,
                        Err(_) => break,
                    };
                    let nid = nid.clone();
                    tokio::spawn(async move {
                        match read_msg(&mut recv).await {
                            Ok(Wire::Offer(o)) => {
                                let _ =
                                    write_msg(&mut send, &Wire::Bid(accept_bid(&o, &nid))).await;
                                let _ = send.finish();
                            }
                            Ok(Wire::Dispatch(d)) => {
                                let _ = write_msg(
                                    &mut send,
                                    &Wire::Ack(Ack {
                                        job_id: d.job_id.clone(),
                                        ok: false,
                                        detail: "at capacity".into(),
                                    }),
                                )
                                .await;
                                let _ = send.finish();
                            }
                            _ => {}
                        }
                    });
                }
            });
        }
    });
    WorkerHandle {
        node_id,
        addr,
        _transport: transport,
        _task: task,
    }
}

// ===========================================================================
// No-hang invariant: a worker that ACCEPTS then REJECTS every dispatch must not
// hang the requester (regression for the pre-existing two-node "at capacity"
// hang — v0.6.0 / PR #2090).
// ===========================================================================

/// A provider that bids "accept" but declines ("at capacity") every dispatch,
/// reached via a candidate with NO stable node id (a bootstrap/TOFU seed
/// address) and re-offered on every attempt. Before the fix this spun forever:
/// the worker's real node id was excluded but the unidentified candidate was
/// re-selected each attempt, and with unlimited retries + a self-refilling retry
/// budget the loop never terminated. The requester MUST now stop with a clear
/// error within a bounded time instead of blocking indefinitely.
#[tokio::test]
async fn admission_rejection_does_not_hang_requester() {
    let rejecter = spawn_at_capacity_worker().await;

    // Same unidentified candidate (node_id = None) returned EVERY attempt — the
    // exact shape of a bootstrap seed that has not yet learned the peer's id.
    let disc = Arc::new(StaticDiscovery::new(
        vec![Candidate::new(None, rejecter.addr)],
        64,
    ));

    // Reproduce the real-world non-terminating regime: unlimited retries, no
    // wall-clock cap, and a retry budget that refills faster than it is spent —
    // so ONLY a correct no-progress bound (not the token bucket) can stop it.
    let mut cfg = fast_cfg(1, 1);
    cfg.scheduler.max_retries = 0; // unlimited
    cfg.scheduler.max_total_duration_ms = 0; // no wall-clock cap
    cfg.scheduler.retry_budget_max_tokens = 64.0;
    cfg.scheduler.retry_budget_refill_per_sec = 100_000.0; // never exhausts
    cfg.validate().unwrap();
    let coord = coord_with(disc, cfg, store()).await;

    // The query must RESOLVE (to an error) within a bounded time — a hang would
    // never complete, so a generous timeout failing the test catches a regression.
    let res = tokio::time::timeout(
        Duration::from_secs(10),
        coord.run_query("SELECT 1", QueryOverrides::default()),
    )
    .await;

    let outcome = res.expect("requester HUNG on an admission rejection (no-hang invariant broken)");
    assert!(
        matches!(
            outcome,
            Err(CoordinatorError::Exhausted { .. })
                | Err(CoordinatorError::NoCandidates)
                | Err(CoordinatorError::InsufficientWorkers { .. })
        ),
        "a rejecting-only provider must terminate with a clear error, got {outcome:?}"
    );
}

fn estimate(scanned: u64, peak: u64) -> WorkingSetEstimate {
    WorkingSetEstimate {
        scanned_uncompressed_bytes: scanned,
        estimated_rows: 0,
        scan_buffer_bytes: 0,
        group_by_bytes: 0,
        join_build_bytes: 0,
        sort_bytes: 0,
        peak_working_set_bytes: peak,
        estimated_runtime_ms: 0,
    }
}

/// A job that OOMs a subset of (low-capacity) nodes re-routes to higher-capacity
/// nodes and SUCCEEDS — consensus `ResourceExceeded` is NOT terminal.
#[tokio::test]
async fn oom_subset_reroutes_to_higher_capacity_and_succeeds() {
    let oom1 = spawn_oom_worker(1 << 20).await; // advertises 1 MiB
    let oom2 = spawn_oom_worker(1 << 20).await;
    let h1 = spawn_worker(Arc::new(MockEngine::deterministic())).await; // 4 GiB free
    let h2 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    // Attempt 1 sees the OOM-prone low-capacity nodes; attempt 2 the big ones.
    let disc = Arc::new(ScriptedDiscovery::new(vec![
        candidates_of(&[&oom1, &oom2]),
        candidates_of(&[&h1, &h2]),
    ]));
    let coord = coord_with(disc, fast_cfg(2, 2), store()).await;

    let outcome = coord
        .run_query("SELECT 1", QueryOverrides::default())
        .await
        .unwrap();
    assert!(outcome.verified, "must succeed after re-routing past the OOMed nodes");
    assert!(
        outcome
            .participants
            .iter()
            .all(|p| *p == h1.node_id || *p == h2.node_id),
        "the job must run on the higher-capacity nodes, not the OOMed ones"
    );
}

/// A job too big for ALL nodes (every node OOMs, none bigger remains) terminates
/// cleanly with the dedicated `ExceedsCapacity` error — after trying the biggest.
#[tokio::test]
async fn job_too_big_for_all_nodes_terminates_exceeds_capacity() {
    let small1 = spawn_oom_worker(1 << 20).await; // 1 MiB
    let small2 = spawn_oom_worker(1 << 20).await;
    let big1 = spawn_oom_worker(100 << 20).await; // 100 MiB — bigger, still OOMs
    let big2 = spawn_oom_worker(100 << 20).await;
    let disc = Arc::new(ScriptedDiscovery::new(vec![
        candidates_of(&[&small1, &small2]),
        candidates_of(&[&big1, &big2]),
        vec![], // nothing higher-capacity remains
    ]));
    let coord = coord_with(disc, fast_cfg(2, 2), store()).await;

    let err = coord
        .run_query("SELECT 1", QueryOverrides::default())
        .await
        .unwrap_err();
    assert!(
        matches!(err, CoordinatorError::ExceedsCapacity { .. }),
        "a job too big for every node must terminate as ExceedsCapacity, got {err:?}"
    );
}

/// A consensus LOGIC error (bad SQL) stays TERMINAL `Infeasible` — it is NOT
/// re-routed (retrying cannot fix a binder error) and never becomes
/// `ExceedsCapacity`.
#[tokio::test]
async fn consensus_logic_error_is_terminal_not_rerouted() {
    let mk = || {
        Arc::new(MockEngine::failing(
            "Binder Error: Referenced column \"nope\" not found",
        )) as Arc<dyn QueryEngine>
    };
    let a = spawn_worker(mk()).await;
    let b = spawn_worker(mk()).await;
    let c = spawn_worker(mk()).await;
    let disc = Arc::new(StaticDiscovery::new(candidates_of(&[&a, &b, &c]), 64));
    let coord = coord_with(disc, fast_cfg(3, 2), store()).await;

    let err = coord
        .run_query("SELECT nope FROM t", QueryOverrides::default())
        .await
        .unwrap_err();
    assert!(
        matches!(err, CoordinatorError::Infeasible { .. }),
        "a logic error must stay terminal (no pointless retries / no reroute), got {err:?}"
    );
}

/// On an `Inconclusive` attempt, only the silent/failed providers are excluded —
/// a node that COMMITTED a correct result but lost the hedged race stays eligible
/// and is reused on the retry (Part A.2).
#[tokio::test]
async fn inconclusive_keeps_correct_but_lost_node_eligible() {
    let h = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let s = spawn_silent_worker().await;
    let h2 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    // Attempt 1: `h` commits but `s` is silent ⇒ quorum (2) not reached ⇒
    // Inconclusive, excluding ONLY `s`. Attempt 2: `h` (still eligible) + `h2`
    // both commit ⇒ success. Were `h` wrongly excluded, attempt 2 would have only
    // `h2` and never reach quorum.
    let disc = Arc::new(ScriptedDiscovery::new(vec![
        candidates_of(&[&h, &s]),
        candidates_of(&[&h, &h2]),
    ]));
    let coord = coord_with(disc, fast_cfg(2, 2), store()).await;

    let outcome = coord
        .run_query("SELECT 1", QueryOverrides::default())
        .await
        .unwrap();
    assert!(outcome.verified);
    assert!(
        outcome.participants.contains(&h.node_id),
        "the correct-but-lost node must stay eligible and be reused on retry"
    );
}

/// The size gate drops a candidate whose advertised capacity clearly cannot hold
/// the job's estimated peak working set — BEFORE dispatch — so an oversize job
/// against an undersized node yields no eligible worker.
#[tokio::test]
async fn capability_gate_excludes_oversize_candidate() {
    let small = spawn_oom_worker(1 << 20).await; // advertises 1 MiB free
    let disc = Arc::new(StaticDiscovery::new(candidates_of(&[&small]), 64));
    let coord = coord_with(disc, fast_cfg(1, 1), store()).await;

    // Estimated peak 8 GiB ≫ 1 MiB (+ spill) ⇒ the only node is gated out.
    let big = estimate(8 << 30, 8 << 30);
    let err = coord
        .run_query_planned("SELECT 1", QueryOverrides::default(), Some(big))
        .await
        .unwrap_err();
    assert!(
        matches!(err, CoordinatorError::InsufficientWorkers { have: 0, .. }),
        "the undersized node must be gated out before dispatch, got {err:?}"
    );
}

/// COLD-START SAFETY: a newcomer with NO proven capability history is NEVER
/// excluded by the size gate — even by a huge SCANNED estimate — as long as it
/// advertises ample free memory. Normal queries therefore never starve.
#[tokio::test]
async fn newcomer_without_history_is_not_excluded_by_size_gate() {
    let h = spawn_worker(Arc::new(MockEngine::deterministic())).await; // 4 GiB free, no history
    let disc = Arc::new(StaticDiscovery::new(candidates_of(&[&h]), 64));
    let coord = coord_with(disc, fast_cfg(1, 1), store()).await;

    // Huge scanned bytes, tiny peak: the proven-max gate must NOT fire (no proven
    // history), and the advertised-memory gate is satisfied by the big free_mem.
    let est = estimate(10 << 30, 1 << 20);
    let outcome = coord
        .run_query_planned("SELECT 1", QueryOverrides::default(), Some(est))
        .await
        .unwrap();
    assert!(outcome.verified);
    assert_eq!(outcome.winner.as_ref(), Some(&h.node_id));
}
