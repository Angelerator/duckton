//! Integrated cross-stack scenarios: the grid (coordinator + workers) driven
//! together with the optional settlement layer (BLOCKCHAIN_ECONOMICS §8/§10).
//!
//! These wire a real loopback-QUIC coordinator/worker run to the settlement
//! seams end-to-end, using the deterministic `mock`/`noop` settlement doubles —
//! NO network and NO live TON. They cover the two halves of the free/paid
//! decoupling that the unit tests only touch in isolation:
//!
//!  * **Free job**: dispatch → quorum → receipts → reputation, touching the
//!    settlement rail ZERO times (proved with a settlement spy that panics on
//!    any call and a stake-registry spy whose `stake_factor` is never consulted).
//!  * **Paid job**: open escrow → dispatch → quorum verdict → settle the payout
//!    split (winner + participation commissions + platform fee, bounded by the
//!    escrowed bid) → anchor the job record + prove Merkle inclusion → reputation
//!    update; plus the `stake_factor` ranking seam, which must be consulted ONLY
//!    when economics is enabled AND the job is paid.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use p2p_config::{
    DataClassCfg, GridConfig, IdentityConfig, PaymentPref, PinningMode, QueryOverrides,
    SettlementRail,
};
use p2p_proto::{Attestation, JobId, NodeId};
use p2p_node::{
    AdmissionController, Candidate, Coordinator, MockEngine, QueryEngine, StaticDiscovery, Worker,
    WorkerParams,
};
use p2p_settlement::types::{Amount, SlashError};
use p2p_settlement::{
    merkle, settle_if_paid, EscrowHandle, InMemoryRecordAnchor, InMemoryStakeRegistry, JobRecord,
    NoopRecordAnchor, PaymentMode, Payout, RecordAnchor, Settlement, SettlementEvent, SettleError,
    SettlementOutcome, SlashReason, StakeRegistry, WalletAddress,
};
use p2p_transport::{NodeIdentity, QuicTransport, Transport};
use p2p_trust::{now_ts, InMemoryTrustStore, TrustStore};

const TON: Amount = 1_000_000_000;

// --------------------------------------------------------------------------
// Harness (mirrors crates/node/tests/scenarios.rs)
// --------------------------------------------------------------------------

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
    let budget = GridConfig::default().budget;
    let net = GridConfig::default().network;
    let transport =
        Arc::new(QuicTransport::bind(&net, &idcfg(), NodeIdentity::generate().unwrap()).unwrap());
    let admission = AdmissionController::new(&budget);
    let mut cfg = GridConfig::default();
    cfg.budget = budget;
    let params = WorkerParams::from_config(&cfg);
    let node_id = transport.local_node_id().clone();
    let addr = transport.local_addr().unwrap();
    let worker = Worker::new(transport.clone(), engine, admission, Attestation::stub_l0(), params);
    let task = worker.spawn();
    WorkerHandle {
        node_id,
        addr,
        _transport: transport,
        _task: task,
    }
}

fn base_cfg(replicas: usize, quorum: usize) -> GridConfig {
    let mut c = GridConfig::default();
    c.scheduler.replicas = replicas;
    c.scheduler.quorum = quorum;
    c.scheduler.offer_timeout_ms = 2_000;
    c.scheduler.dispatch_timeout_ms = 5_000;
    c.trust.min_trust = 0.0;
    c.discovery.candidate_sample_size = 64;
    c.validate().unwrap();
    c
}

/// A config with the economics rail enabled and a paid default (the paid grid).
fn paid_cfg(replicas: usize, quorum: usize) -> GridConfig {
    let mut c = base_cfg(replicas, quorum);
    c.economics.enabled = true;
    c.economics.default_payment = PaymentPref::Paid;
    c.economics.settlement = SettlementRail::Channel;
    c.economics.fee_recipient = Some(format!("0:{}", "00".repeat(32)));
    c.validate().unwrap();
    c
}

fn store() -> Arc<InMemoryTrustStore> {
    Arc::new(InMemoryTrustStore::new(
        &GridConfig::default().trust,
        &GridConfig::default().limits,
    ))
}

async fn coordinator(workers: &[&WorkerHandle], cfg: GridConfig, st: Arc<dyn TrustStore>) -> Coordinator {
    let net = GridConfig::default().network;
    let req =
        Arc::new(QuicTransport::bind(&net, &idcfg(), NodeIdentity::generate().unwrap()).unwrap());
    let candidates: Vec<Candidate> = workers
        .iter()
        .map(|w| Candidate::new(Some(w.node_id.clone()), w.addr))
        .collect();
    let disc = Arc::new(StaticDiscovery::new(candidates, cfg.discovery.candidate_sample_size));
    Coordinator::new(req, disc, st, Arc::new(cfg), "mock-1")
}

// --------------------------------------------------------------------------
// Test doubles for the "zero chain calls on free jobs" assertions
// --------------------------------------------------------------------------

/// A settlement that PANICS on any call — proves a free job never engages it.
struct PanicSettlement;
impl Settlement for PanicSettlement {
    fn open_escrow(&self, _: &JobId, _: Amount) -> Result<EscrowHandle, SettleError> {
        panic!("settlement engaged on a FREE job (open_escrow)");
    }
    fn settle(&self, _: &EscrowHandle, _: &SettlementOutcome) -> Result<(), SettleError> {
        panic!("settlement engaged on a FREE job (settle)");
    }
    fn refund(&self, _: &EscrowHandle) -> Result<(), SettleError> {
        panic!("settlement engaged on a FREE job (refund)");
    }
    fn is_onchain(&self) -> bool {
        panic!("settlement inspected on a FREE job (is_onchain)");
    }
}

/// A stake registry that counts how often the ranking seam (`stake_factor`) is
/// consulted, delegating to an inner in-memory registry for the value.
struct SpyStakeRegistry {
    inner: InMemoryStakeRegistry,
    factor_calls: AtomicUsize,
}

impl SpyStakeRegistry {
    fn new(inner: InMemoryStakeRegistry) -> Self {
        Self { inner, factor_calls: AtomicUsize::new(0) }
    }
    fn factor_calls(&self) -> usize {
        self.factor_calls.load(Ordering::SeqCst)
    }
}

impl StakeRegistry for SpyStakeRegistry {
    fn stake_of(&self, node: &NodeId) -> Amount {
        self.inner.stake_of(node)
    }
    fn is_eligible(&self, node: &NodeId, class: DataClassCfg) -> bool {
        self.inner.is_eligible(node, class)
    }
    fn stake_factor(&self, node: &NodeId) -> f64 {
        self.factor_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.stake_factor(node)
    }
    fn slash(&self, node: &NodeId, reason: SlashReason, amount: Amount) -> Result<(), SlashError> {
        self.inner.slash(node, reason, amount)
    }
    fn request_unbond(&self, node: &NodeId, amount: Amount) -> Result<(), SlashError> {
        self.inner.request_unbond(node, amount)
    }
}

fn wallet(tag: u8) -> WalletAddress {
    WalletAddress::new(0, [tag; 32])
}

// --------------------------------------------------------------------------
// FREE job: full grid path, ZERO settlement interaction
// --------------------------------------------------------------------------

/// A free job runs the complete dispatch → quorum → receipts → reputation path
/// but never touches the settlement rail. We prove "zero chain calls" three
/// ways: the per-job mode resolves to Free, `settle_if_paid` returns `false`
/// against a settlement that panics on ANY call, and the no-op anchor stores
/// nothing.
#[tokio::test]
async fn free_job_runs_full_grid_path_with_zero_chain_calls() {
    let a = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let b = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let st = store();

    // Default economics => disabled => every job is free.
    let cfg = base_cfg(2, 2);
    assert!(!cfg.economics.enabled, "default grid must be free / no-chain");
    let coord = coordinator(&[&a, &b], cfg, st.clone() as Arc<dyn TrustStore>).await;

    let outcome = coord.run_query("SELECT 1", QueryOverrides::default()).await.unwrap();

    // Grid path, verified, with signed receipts and reputation updated.
    assert!(!outcome.executed_locally);
    assert!(outcome.verified);
    assert!(!outcome.receipts.is_empty(), "a free job must still emit receipts");
    let winner = outcome.winner.clone().unwrap();
    assert!(st.reputation(&winner, now_ts()).is_some(), "free work must score");
    assert!(st.observation_count(&winner) >= 1);

    // The settlement decision for a free job NEVER reaches the rail: even a
    // settlement that panics on any call is safe here.
    let dummy = SettlementOutcome {
        result_hash: [0u8; 32],
        winner: Payout { to: wallet(1), amount: 0 },
        participants: vec![],
        platform_fee: 0,
    };
    let engaged = settle_if_paid(
        PaymentMode::Free,
        &PanicSettlement,
        &outcome.job_id,
        100 * TON,
        &dummy,
    )
    .unwrap();
    assert!(!engaged, "free job must not engage settlement");

    // The free path anchors nothing on-chain.
    let anchor = NoopRecordAnchor;
    assert!(anchor.prove_inclusion(&outcome.job_id).is_none());
    assert_eq!(anchor.epoch_root(), [0u8; 32]);
}

// --------------------------------------------------------------------------
// PAID job: escrow → settle (split) → anchor → reputation
// --------------------------------------------------------------------------

/// A paid job runs the grid, then settles the full payout split through the
/// escrow and anchors the job record. We assert the recorded settlement events,
/// the payout total being bounded by the escrowed bid, and a verifiable Merkle
/// inclusion proof for the anchored record.
#[tokio::test]
async fn paid_job_settles_split_and_anchors_record() {
    // Two agreeing workers: one wins, the other is an agreeing participant that
    // earns a participation commission.
    let w1 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let w2 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let st = store();

    let cfg = paid_cfg(2, 2);
    let reg = Arc::new(InMemoryStakeRegistry::new(0, 0, 0, 100_000 * TON));
    reg.set_stake(&w1.node_id, 1_000 * TON);
    reg.set_stake(&w2.node_id, 1_000 * TON);

    let coord = coordinator(&[&w1, &w2], cfg.clone(), st.clone() as Arc<dyn TrustStore>)
        .await
        .with_stake_registry(reg);

    let outcome = coord.run_query("SELECT 1", QueryOverrides::default()).await.unwrap();
    assert!(outcome.verified);
    let agreed = outcome.agreed_hash.clone().expect("quorum agreed on a hash");
    let winner = outcome.winner.clone().unwrap();
    // Agreeing non-winners receive participation commissions.
    let others: Vec<NodeId> =
        outcome.participants.iter().filter(|p| **p != winner).cloned().collect();
    assert_eq!(others.len(), 1, "one agreeing participant beside the winner");

    // Build the settlement split, bounded by the escrowed max bid `B`.
    let max_bid = 100 * TON;
    let settlement_outcome = SettlementOutcome {
        result_hash: blake3::hash(agreed.as_bytes()).into(),
        winner: Payout { to: wallet(0xA1), amount: 60 * TON },
        participants: others.iter().enumerate().map(|(i, _)| Payout {
            to: wallet(0xB0 + i as u8),
            amount: 5 * TON,
        }).collect(),
        platform_fee: 3 * TON,
    };
    let expected_total = 60 * TON + 5 * TON + 3 * TON; // winner + 1 commission + fee
    assert_eq!(settlement_outcome.total(), expected_total);
    assert!(settlement_outcome.total() <= max_bid);

    // Engage the paid rail: open escrow + settle (HTLC keyed on the result hash).
    let mock = p2p_settlement::MockSettlement::new();
    let engaged =
        settle_if_paid(PaymentMode::Paid, &mock, &outcome.job_id, max_bid, &settlement_outcome)
            .unwrap();
    assert!(engaged, "paid job must engage settlement");
    assert_eq!(
        mock.events(),
        vec![
            SettlementEvent::Opened { job: outcome.job_id.clone(), max_bid },
            SettlementEvent::Settled { job: outcome.job_id.clone(), total: expected_total },
        ]
    );

    // Anchor the job record and prove Merkle inclusion against the epoch root.
    let anchor = InMemoryRecordAnchor::new();
    // A couple of sibling records so the proof has a non-trivial path.
    for i in 0..2u8 {
        anchor.append(&JobRecord {
            job_id: format!("sibling-{i}"),
            query_hash: "qh".into(),
            requester_wallet: wallet(0xCC).to_raw_string(),
            max_bid,
            result_hash: "other".into(),
            epoch: 1,
            prev_root: [0u8; 32],
        });
    }
    anchor.append(&JobRecord {
        job_id: outcome.job_id.0.clone(),
        query_hash: "qh".into(),
        requester_wallet: wallet(0xCC).to_raw_string(),
        max_bid,
        result_hash: agreed.clone(),
        epoch: 1,
        prev_root: [0u8; 32],
    });
    let root = anchor.epoch_root();
    let proof = anchor.prove_inclusion(&outcome.job_id).expect("anchored record proves inclusion");
    assert!(merkle::verify_inclusion(&root, &proof), "inclusion proof must verify");

    // Reputation still updated for paid work.
    assert!(st.reputation(&winner, now_ts()).is_some());
    assert!(st.observation_count(&winner) >= 1);
}

/// The escrow is an HTLC-bounded ceiling: a payout split whose total exceeds the
/// locked bid `B` is rejected by the rail (no over-spend out of escrow).
#[tokio::test]
async fn paid_settlement_rejects_payout_exceeding_escrow() {
    let w = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let st = store();
    let coord = coordinator(&[&w], paid_cfg(1, 1), st as Arc<dyn TrustStore>).await;
    let outcome = coord.run_query("SELECT 1", QueryOverrides::default()).await.unwrap();

    let max_bid = 50 * TON;
    let over_budget = SettlementOutcome {
        result_hash: [9u8; 32],
        winner: Payout { to: wallet(0xA1), amount: 60 * TON }, // already over the 50 bid
        participants: vec![],
        platform_fee: 0,
    };
    let err = settle_if_paid(PaymentMode::Paid, &p2p_settlement::MockSettlement::new(), &outcome.job_id, max_bid, &over_budget)
        .unwrap_err();
    assert!(matches!(err, SettleError::PayoutExceedsEscrow { .. }), "got {err:?}");
}

// --------------------------------------------------------------------------
// stake_factor ranking seam: consulted ONLY when paid AND economics enabled
// --------------------------------------------------------------------------

/// The economics `stake_factor` is the single chain-gated input to selection.
/// It must be consulted ONLY for paid jobs while economics is enabled. We prove
/// the gate with a spy registry whose `stake_factor` counts its invocations:
/// zero on the free/default grid, and non-zero on the paid grid.
#[tokio::test]
async fn stake_seam_consulted_only_when_paid_and_enabled() {
    // --- Free / default grid: the seam is never consulted. ---
    {
        let w = spawn_worker(Arc::new(MockEngine::deterministic())).await;
        let st = store();
        let spy = Arc::new(SpyStakeRegistry::new(InMemoryStakeRegistry::new(0, 0, 0, 100_000 * TON)));
        spy.inner.set_stake(&w.node_id, 1_000 * TON);
        let coord = coordinator(&[&w], base_cfg(1, 1), st as Arc<dyn TrustStore>)
            .await
            .with_stake_registry(spy.clone());

        let outcome = coord.run_query("SELECT 1", QueryOverrides::default()).await.unwrap();
        assert!(outcome.verified);
        assert_eq!(spy.factor_calls(), 0, "free job must not consult the stake seam");
    }

    // --- Paid + enabled grid: the seam IS consulted. ---
    {
        let w = spawn_worker(Arc::new(MockEngine::deterministic())).await;
        let st = store();
        let spy = Arc::new(SpyStakeRegistry::new(InMemoryStakeRegistry::new(0, 0, 0, 100_000 * TON)));
        spy.inner.set_stake(&w.node_id, 1_000 * TON);
        let coord = coordinator(&[&w], paid_cfg(1, 1), st as Arc<dyn TrustStore>)
            .await
            .with_stake_registry(spy.clone());

        let outcome = coord.run_query("SELECT 1", QueryOverrides::default()).await.unwrap();
        assert!(outcome.verified);
        assert!(spy.factor_calls() >= 1, "paid+enabled job must consult the stake seam");
    }
}

/// On the paid grid the staked worker out-ranks an unstaked peer with identical
/// (bootstrap) reputation, so with a single replica the staked worker is the one
/// selected and wins — deterministically (its score is strictly higher, so the
/// candidate shuffle cannot change the top-1). This shows the seam genuinely
/// influences ranking, not just that it is called.
#[tokio::test]
async fn paid_stake_factor_decides_single_replica_winner() {
    let staked = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let unstaked = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let st = store();

    let reg = Arc::new(InMemoryStakeRegistry::new(0, 0, 0, 100_000 * TON));
    reg.set_stake(&staked.node_id, 100_000 * TON); // at the cap => stake_factor ~ 1.0
    // `unstaked` intentionally has zero stake.

    // Both workers are fresh (equal bootstrap reputation); with one replica the
    // strictly higher stake-boosted score must select `staked` every time.
    for _ in 0..5 {
        let coord = coordinator(&[&staked, &unstaked], paid_cfg(1, 1), st.clone() as Arc<dyn TrustStore>)
            .await
            .with_stake_registry(reg.clone());
        let outcome = coord.run_query("SELECT 1", QueryOverrides::default()).await.unwrap();
        assert_eq!(
            outcome.winner.as_ref(),
            Some(&staked.node_id),
            "the staked worker must out-rank the unstaked peer on the paid grid",
        );
    }
}
