//! Integration tests for the anti-abuse / robustness layer (ARCHITECTURE "Abuse
//! resistance"): fault attribution + non-determinism handling, requester-trust
//! weighting, pre-flight cost gating, deny-list enforcement, and free-mode rate
//! limiting. Each mechanism is exercised end-to-end over the real coordinator /
//! worker bid path (loopback QUIC where networking is involved).

use std::net::SocketAddr;
use std::sync::Arc;

use p2p_config::{GridConfig, IdentityConfig, PinningMode};
use p2p_node::{
    AdmissionController, Blocklist, Candidate, Coordinator, MockEngine, QueryEngine, RateLimiter,
    StaticDiscovery, Worker, WorkerParams,
};
use p2p_proto::{Attestation, BidDecision, DataClass, JobId, NodeId, Offer, QueryHash};
use p2p_transport::{NodeIdentity, QuicTransport, Transport};
use p2p_trust::{now_ts, InMemoryTrustStore, TrustStore};

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

fn store() -> Arc<InMemoryTrustStore> {
    Arc::new(InMemoryTrustStore::new(
        &GridConfig::default().trust,
        &GridConfig::default().limits,
    ))
}

async fn spawn_worker(engine: Arc<dyn QueryEngine>) -> WorkerHandle {
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
    );
    let task = worker.spawn();
    WorkerHandle {
        node_id,
        addr,
        _transport: transport,
        _task: task,
    }
}

/// A coordinator over the given workers and config, returning the coordinator
/// plus its own requester node id (so tests can seed requester reputation).
async fn coordinator(
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

/// Base test config with the trust gate relaxed so fresh workers are selectable.
fn base_cfg(replicas: usize, quorum: usize) -> GridConfig {
    let mut c = GridConfig::default();
    c.scheduler.replicas = replicas;
    c.scheduler.quorum = quorum;
    c.scheduler.offer_timeout_ms = 2_000;
    c.scheduler.dispatch_timeout_ms = 5_000;
    c.trust.min_trust = 0.0;
    c.discovery.candidate_sample_size = 16;
    c
}

/// Build a (non-spawned) worker for direct `bid_for` evaluation, with anti-abuse
/// wired from `cfg`.
fn worker_for_bids(
    cfg: &GridConfig,
    blocklist: Option<Arc<Blocklist>>,
    rl: Option<Arc<RateLimiter>>,
) -> Worker {
    let transport = Arc::new(
        QuicTransport::bind(&cfg.network, &idcfg(), NodeIdentity::generate().unwrap()).unwrap(),
    );
    let admission = AdmissionController::new(&cfg.budget);
    let params = WorkerParams::from_config(cfg);
    Worker::new(
        transport,
        Arc::new(MockEngine::deterministic()),
        admission,
        Attestation::stub_l0(),
        params,
    )
    .with_antiabuse(cfg, blocklist, rl)
}

fn offer(requester: &str, cost_hint_rows: Option<u64>, class: DataClass) -> Offer {
    Offer {
        job_id: JobId::new(),
        requester_id: NodeId(requester.into()),
        query_hash: QueryHash::compute("SELECT 1", "mock-1"),
        cost_hint_rows,
        data_class: class,
        nonce: 1,
        network: None,
        groups: Vec::new(),
        regions: Vec::new(),
    }
}

// --------------------------------------------------------------------------
// 1. Fault attribution + non-determinism: a non-verifiable query never
//    penalizes a provider (even one returning a different hash).
// --------------------------------------------------------------------------
#[tokio::test]
async fn nondeterministic_query_marks_non_verifiable_and_applies_no_penalty() {
    let h1 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let h2 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let bad = spawn_worker(Arc::new(MockEngine::deterministic().cheating())).await;
    let st = store();
    let cfg = Arc::new(base_cfg(3, 2)); // antiabuse.nondeterminism on by default
    let coord = coordinator(&[&h1, &h2, &bad], cfg, st.clone() as Arc<dyn TrustStore>).await;

    // `random()` cannot reach a stable quorum hash → non-verifiable.
    let outcome = coord
        .run_query("SELECT random() AS r", Default::default())
        .await
        .expect("a non-verifiable query still returns a result");
    assert!(
        !outcome.verified,
        "non-deterministic query must be flagged non-verified"
    );
    // No provider is penalized — not even the 'cheater' (its divergent hash is
    // expected for a non-deterministic query, not provable fault).
    assert_eq!(st.penalty(&bad.node_id), 0.0);
    assert_eq!(st.penalty(&h1.node_id), 0.0);
    // And no reputation observation was recorded for any provider.
    assert_eq!(st.observation_count(&bad.node_id), 0);
}

// --------------------------------------------------------------------------
// 2. Requester-trust weighting: a brand-new requester's negative outcome barely
//    moves a provider's score; an established requester's does.
// --------------------------------------------------------------------------
#[tokio::test]
async fn requester_trust_weighting_gates_new_senders() {
    // Fresh requester (no requester history): an incorrect outcome applies ~0
    // penalty because w(requester) ≈ 0 for negatives.
    let mut cfg = base_cfg(3, 2);
    cfg.antiabuse.requester_trust.enabled = true;
    cfg.antiabuse.requester_trust.negative_floor_weight = 0.0;
    cfg.antiabuse.requester_trust.age_saturation = 50;
    let cfg = Arc::new(cfg);

    let h1 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let h2 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let bad = spawn_worker(Arc::new(MockEngine::deterministic().cheating())).await;
    let st = store();
    let coord = coordinator(
        &[&h1, &h2, &bad],
        cfg.clone(),
        st.clone() as Arc<dyn TrustStore>,
    )
    .await;
    let outcome = coord
        .run_query("SELECT 1", Default::default())
        .await
        .unwrap();
    assert!(outcome.verified, "two honest workers still reach quorum");
    // The cheater is correctly identified...
    let bad_receipt = outcome
        .receipts
        .iter()
        .find(|r| r.worker_id == bad.node_id)
        .unwrap();
    assert_eq!(bad_receipt.verdict, p2p_proto::Verdict::Incorrect);
    // ...but a brand-new requester barely moves its score.
    assert_eq!(
        st.penalty(&bad.node_id),
        0.0,
        "new requester's penalty is gated to ~0"
    );

    // Established requester: seed this node's own requester reputation, then the
    // SAME cheater outcome applies the full penalty.
    let st2 = store();
    let coord2 = coordinator(
        &[&h1, &h2, &bad],
        cfg.clone(),
        st2.clone() as Arc<dyn TrustStore>,
    )
    .await;
    let self_id = coord2.local_node_id().clone();
    let now = now_ts();
    for _ in 0..60 {
        st2.record_requester(&self_id, true, now);
    }
    let outcome2 = coord2
        .run_query("SELECT 1", Default::default())
        .await
        .unwrap();
    assert!(outcome2.verified);
    let penalty = st2.penalty(&bad.node_id);
    assert!(
        penalty > 0.4,
        "an established requester applies the full penalty, got {penalty}"
    );
}

// --------------------------------------------------------------------------
// 3. Pre-flight cost gating: an over-budget offer is declined up front (not
//    executed, so the provider's score is untouched).
// --------------------------------------------------------------------------
#[tokio::test]
async fn cost_gate_declines_over_budget_offer() {
    let mut cfg = base_cfg(3, 2);
    cfg.antiabuse.cost_gate.enabled = true;
    cfg.antiabuse.cost_gate.max_cost_hint_rows = 1_000_000;
    let worker = worker_for_bids(&cfg, None, None);

    // Within budget ⇒ accepted.
    let ok = worker.bid_for(&offer("b3:req", Some(500_000), DataClass::Public));
    assert!(
        matches!(ok.decision, BidDecision::Accept),
        "within budget should accept"
    );

    // Heavy query ⇒ declined (rejection, not a failure — no execution, no score).
    let heavy = worker.bid_for(&offer("b3:req", Some(50_000_000), DataClass::Public));
    match heavy.decision {
        BidDecision::Reject { reason } => assert!(reason.contains("over budget"), "got {reason}"),
        other => panic!("expected reject, got {other:?}"),
    }
}

// --------------------------------------------------------------------------
// 4. Deny-list enforcement: a blocked candidate is excluded from selection;
//    a blocked requester is refused by the worker.
// --------------------------------------------------------------------------
#[tokio::test]
async fn blocklist_excludes_blocked_candidate_from_selection() {
    let good = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let blocked = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let st = store();
    let cfg = Arc::new(base_cfg(1, 1));
    let bl = Arc::new(Blocklist::new());
    bl.block(
        blocked.node_id.as_str(),
        p2p_config::BlockKind::NodeId,
        "test",
        "manual",
    );
    let coord = coordinator(&[&good, &blocked], cfg, st as Arc<dyn TrustStore>)
        .await
        .with_blocklist(bl);

    let outcome = coord
        .run_query("SELECT 1", Default::default())
        .await
        .unwrap();
    // The blocked worker must never be selected.
    assert!(!outcome.participants.contains(&blocked.node_id));
    assert_eq!(outcome.winner.as_ref(), Some(&good.node_id));
}

#[tokio::test]
async fn worker_refuses_blocked_requester() {
    let cfg = base_cfg(3, 2);
    let bl = Arc::new(Blocklist::new());
    bl.block("b3:evil", p2p_config::BlockKind::NodeId, "abuse", "manual");
    let worker = worker_for_bids(&cfg, Some(bl), None);

    let rejected = worker.bid_for(&offer("b3:evil", None, DataClass::Public));
    match rejected.decision {
        BidDecision::Reject { reason } => assert!(reason.contains("blocklist"), "got {reason}"),
        other => panic!("expected reject for blocked requester, got {other:?}"),
    }
    // A different requester is still served.
    let ok = worker.bid_for(&offer("b3:friendly", None, DataClass::Public));
    assert!(matches!(ok.decision, BidDecision::Accept));
}

// --------------------------------------------------------------------------
// 5. Free-mode rate limiting: a free requester is throttled after its budget.
// --------------------------------------------------------------------------
#[tokio::test]
async fn free_mode_rate_limit_triggers_per_requester() {
    let mut cfg = base_cfg(3, 2);
    cfg.antiabuse.free_rate_limit.enabled = true;
    cfg.antiabuse.free_rate_limit.max_free_per_window = 2;
    cfg.antiabuse.free_rate_limit.window_secs = 60;
    // economics disabled ⇒ every job is free ⇒ the limiter applies.
    let rl = Arc::new(RateLimiter::new(2, 60, 100));
    let worker = worker_for_bids(&cfg, None, Some(rl));

    // First two free offers accepted...
    assert!(matches!(
        worker
            .bid_for(&offer("b3:spammer", None, DataClass::Public))
            .decision,
        BidDecision::Accept
    ));
    assert!(matches!(
        worker
            .bid_for(&offer("b3:spammer", None, DataClass::Public))
            .decision,
        BidDecision::Accept
    ));
    // ...the third is rate-limited.
    match worker
        .bid_for(&offer("b3:spammer", None, DataClass::Public))
        .decision
    {
        BidDecision::Reject { reason } => assert!(reason.contains("rate limit"), "got {reason}"),
        other => panic!("expected rate-limit reject, got {other:?}"),
    }
    // A different requester identity has its own budget.
    assert!(matches!(
        worker
            .bid_for(&offer("b3:other", None, DataClass::Public))
            .decision,
        BidDecision::Accept
    ));
}

// Sanity: with everything default-off-where-observable, a normal deterministic
// cheater is still penalized (the always-safe pieces don't weaken detection).
#[tokio::test]
async fn deterministic_cheater_still_penalized_by_default() {
    let h1 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let h2 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let bad = spawn_worker(Arc::new(MockEngine::deterministic().cheating())).await;
    let st = store();
    let coord = coordinator(
        &[&h1, &h2, &bad],
        Arc::new(base_cfg(3, 2)),
        st.clone() as Arc<dyn TrustStore>,
    )
    .await;
    let outcome = coord
        .run_query("SELECT 1", Default::default())
        .await
        .unwrap();
    assert!(outcome.verified);
    assert!(
        st.penalty(&bad.node_id) > 0.0,
        "a real cheater is still penalized"
    );
}

#[cfg(feature = "discovery-libp2p")]
#[tokio::test]
async fn gossipsub_peer_scoring_config_plumbs_and_builds() {
    use p2p_node::{Libp2pDiscovery, Libp2pDiscoveryConfig};
    use std::time::Duration;

    let mut cfg = GridConfig::default();
    cfg.antiabuse.gossip.peer_scoring = true;
    let dc = Libp2pDiscoveryConfig::from_grid(&cfg).unwrap();
    assert!(
        dc.gossip_peer_scoring,
        "peer scoring flag must plumb from config"
    );

    // The overlay builds and listens with peer scoring enabled.
    let disc = Libp2pDiscovery::spawn(dc)
        .await
        .expect("overlay builds with peer scoring");
    let addrs = disc.wait_listeners(Duration::from_secs(5)).await;
    assert!(!addrs.is_empty(), "overlay should bind a listen address");
}
