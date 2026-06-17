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

use std::time::Duration;

use p2p_config::{
    DataClassCfg, GridConfig, IdentityConfig, PaymentPref, PinningMode, PricingModel,
    QueryOverrides, SettlementRail,
};
use p2p_node::{
    AdmissionController, Candidate, Coordinator, MockEngine, QueryEngine, StaticDiscovery, Worker,
    WorkerParams, WorkingSetEstimate,
};
use p2p_proto::{Attestation, JobId, NodeId, QueryHash, Receipt, Verdict};
use p2p_settlement::types::{Amount, SlashError};
use p2p_settlement::{
    merkle, settle_if_paid, EscrowHandle, Hash32, InMemoryRecordAnchor, InMemoryStakeRegistry,
    InclusionProof, JobRecord, NoopRecordAnchor, OnchainPolicy, ParamsSource, PaymentMode, Payout,
    RecordAnchor, SettleError, Settlement, SettlementEvent, SettlementOutcome, SlashReason,
    StakeRegistry, WalletAddress,
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
    spawn_worker_att(engine, Attestation::stub_l0()).await
}

/// Like [`spawn_worker`] but lets the test set the worker's advertised
/// attestation (to exercise the requester-side verified-level gate).
async fn spawn_worker_att(engine: Arc<dyn QueryEngine>, attestation: Attestation) -> WorkerHandle {
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
    let worker = Worker::new(transport.clone(), engine, admission, attestation, params);
    let task = worker.spawn();
    WorkerHandle {
        node_id,
        addr,
        _transport: transport,
        _task: task,
    }
}

/// Like [`spawn_worker`] but wires a real (software) attestor so the worker
/// emits PER-OFFER, nonce-bound attestation evidence (the honest L1/L2 path).
async fn spawn_worker_attestor(
    engine: Arc<dyn QueryEngine>,
    attestor: Arc<dyn p2p_trust::attestation::Attestor>,
    bound_pub: [u8; 32],
) -> WorkerHandle {
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
    let worker = Worker::new(
        transport.clone(),
        engine,
        admission,
        Attestation::stub_l0(),
        params,
    )
    .with_attestor(attestor, bound_pub);
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
    // A concrete max bid so the per-job escrow locks a known, bounded B.
    c.economics.pricing.max_bid = 100; // whole TON
    c.validate().unwrap();
    c
}

fn store() -> Arc<InMemoryTrustStore> {
    Arc::new(InMemoryTrustStore::new(
        &GridConfig::default().trust,
        &GridConfig::default().limits,
    ))
}

async fn coordinator(
    workers: &[&WorkerHandle],
    cfg: GridConfig,
    st: Arc<dyn TrustStore>,
) -> Coordinator {
    let net = GridConfig::default().network;
    let req =
        Arc::new(QuicTransport::bind(&net, &idcfg(), NodeIdentity::generate().unwrap()).unwrap());
    let candidates: Vec<Candidate> = workers
        .iter()
        .map(|w| Candidate::new(Some(w.node_id.clone()), w.addr))
        .collect();
    let disc = Arc::new(StaticDiscovery::new(
        candidates,
        cfg.discovery.candidate_sample_size,
    ));
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
        Self {
            inner,
            factor_calls: AtomicUsize::new(0),
        }
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
    assert!(
        !cfg.economics.enabled,
        "default grid must be free / no-chain"
    );
    let coord = coordinator(&[&a, &b], cfg, st.clone() as Arc<dyn TrustStore>).await;

    let outcome = coord
        .run_query("SELECT 1", QueryOverrides::default())
        .await
        .unwrap();

    // Grid path, verified, with signed receipts and reputation updated.
    assert!(!outcome.executed_locally);
    assert!(outcome.verified);
    assert!(
        !outcome.receipts.is_empty(),
        "a free job must still emit receipts"
    );
    let winner = outcome.winner.clone().unwrap();
    assert!(
        st.reputation(&winner, now_ts()).is_some(),
        "free work must score"
    );
    assert!(st.observation_count(&winner) >= 1);

    // The settlement decision for a free job NEVER reaches the rail: even a
    // settlement that panics on any call is safe here.
    let dummy = SettlementOutcome {
        result_hash: [0u8; 32],
        base: 0,
        winner: Payout {
            to: wallet(1),
            amount: 0,
        },
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

    let outcome = coord
        .run_query("SELECT 1", QueryOverrides::default())
        .await
        .unwrap();
    assert!(outcome.verified);
    let agreed = outcome
        .agreed_hash
        .clone()
        .expect("quorum agreed on a hash");
    let winner = outcome.winner.clone().unwrap();
    // Agreeing non-winners receive participation commissions.
    let others: Vec<NodeId> = outcome
        .participants
        .iter()
        .filter(|p| **p != winner)
        .cloned()
        .collect();
    assert_eq!(
        others.len(),
        1,
        "one agreeing participant beside the winner"
    );

    // Build the settlement split, bounded by the escrowed max bid `B`.
    let max_bid = 100 * TON;
    let settlement_outcome = SettlementOutcome {
        result_hash: blake3::hash(agreed.as_bytes()).into(),
        base: 60 * TON,
        winner: Payout {
            to: wallet(0xA1),
            amount: 60 * TON,
        },
        participants: others
            .iter()
            .enumerate()
            .map(|(i, _)| Payout {
                to: wallet(0xB0 + i as u8),
                amount: 5 * TON,
            })
            .collect(),
        platform_fee: 3 * TON,
    };
    let expected_total = 60 * TON + 5 * TON + 3 * TON; // winner + 1 commission + fee
    assert_eq!(settlement_outcome.total(), expected_total);
    assert!(settlement_outcome.total() <= max_bid);

    // Engage the paid rail: open escrow + settle (HTLC keyed on the result hash).
    let mock = p2p_settlement::MockSettlement::new();
    let engaged = settle_if_paid(
        PaymentMode::Paid,
        &mock,
        &outcome.job_id,
        max_bid,
        &settlement_outcome,
    )
    .unwrap();
    assert!(engaged, "paid job must engage settlement");
    assert_eq!(
        mock.events(),
        vec![
            SettlementEvent::Opened {
                job: outcome.job_id.clone(),
                max_bid
            },
            SettlementEvent::Settled {
                job: outcome.job_id.clone(),
                total: expected_total
            },
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
            params_version: 0,
            input_fingerprint: String::new(),
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
        params_version: 0,
        input_fingerprint: String::new(),
    });
    let root = anchor.epoch_root();
    let proof = anchor
        .prove_inclusion(&outcome.job_id)
        .expect("anchored record proves inclusion");
    assert!(
        merkle::verify_inclusion(&root, &proof),
        "inclusion proof must verify"
    );

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
    let outcome = coord
        .run_query("SELECT 1", QueryOverrides::default())
        .await
        .unwrap();

    let max_bid = 50 * TON;
    let over_budget = SettlementOutcome {
        result_hash: [9u8; 32],
        base: 60 * TON,
        winner: Payout {
            to: wallet(0xA1),
            amount: 60 * TON,
        }, // already over the 50 bid
        participants: vec![],
        platform_fee: 0,
    };
    let err = settle_if_paid(
        PaymentMode::Paid,
        &p2p_settlement::MockSettlement::new(),
        &outcome.job_id,
        max_bid,
        &over_budget,
    )
    .unwrap_err();
    assert!(
        matches!(err, SettleError::PayoutExceedsEscrow { .. }),
        "got {err:?}"
    );
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
        let spy = Arc::new(SpyStakeRegistry::new(InMemoryStakeRegistry::new(
            0,
            0,
            0,
            100_000 * TON,
        )));
        spy.inner.set_stake(&w.node_id, 1_000 * TON);
        let coord = coordinator(&[&w], base_cfg(1, 1), st as Arc<dyn TrustStore>)
            .await
            .with_stake_registry(spy.clone());

        let outcome = coord
            .run_query("SELECT 1", QueryOverrides::default())
            .await
            .unwrap();
        assert!(outcome.verified);
        assert_eq!(
            spy.factor_calls(),
            0,
            "free job must not consult the stake seam"
        );
    }

    // --- Paid + enabled grid: the seam IS consulted. ---
    {
        let w = spawn_worker(Arc::new(MockEngine::deterministic())).await;
        let st = store();
        let spy = Arc::new(SpyStakeRegistry::new(InMemoryStakeRegistry::new(
            0,
            0,
            0,
            100_000 * TON,
        )));
        spy.inner.set_stake(&w.node_id, 1_000 * TON);
        let coord = coordinator(&[&w], paid_cfg(1, 1), st as Arc<dyn TrustStore>)
            .await
            .with_stake_registry(spy.clone());

        let outcome = coord
            .run_query("SELECT 1", QueryOverrides::default())
            .await
            .unwrap();
        assert!(outcome.verified);
        assert!(
            spy.factor_calls() >= 1,
            "paid+enabled job must consult the stake seam"
        );
    }
}

// --------------------------------------------------------------------------
// Coordinator-DRIVEN engagement: the live run_query path itself opens escrow,
// settles per the verdict, and anchors the record (not a standalone helper call).
// --------------------------------------------------------------------------

/// A PAID grid job, run through the real coordinator with a settlement + anchor
/// rail wired, must itself: open escrow for B → settle the payout split (bounded
/// by B) → append the JobRecord. We assert the recorded settlement events and
/// that the anchored record proves inclusion — all driven by `run_query`, with
/// nothing called by the test beyond `run_query`.
#[tokio::test]
async fn paid_job_drives_coordinator_open_settle_anchor() {
    let w1 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let w2 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let st = store();
    let cfg = paid_cfg(2, 2);

    let reg = Arc::new(InMemoryStakeRegistry::new(0, 0, 0, 100_000 * TON));
    reg.set_stake(&w1.node_id, 1_000 * TON);
    reg.set_stake(&w2.node_id, 1_000 * TON);
    let settlement = Arc::new(p2p_settlement::MockSettlement::new());
    let anchor = Arc::new(InMemoryRecordAnchor::new());

    let coord = coordinator(&[&w1, &w2], cfg, st.clone() as Arc<dyn TrustStore>)
        .await
        .with_stake_registry(reg)
        .with_settlement(settlement.clone())
        .with_record_anchor(anchor.clone());

    let outcome = coord
        .run_query("SELECT 1", QueryOverrides::default())
        .await
        .unwrap();
    assert!(outcome.verified);
    let winner = outcome.winner.clone().unwrap();

    // The coordinator opened escrow for B (max_bid 100 TON) and then settled.
    let max_bid = 100 * TON;
    let events = settlement.events();
    assert_eq!(
        events.len(),
        2,
        "open then settle (no refund), got {events:?}"
    );
    assert!(
        matches!(&events[0], SettlementEvent::Opened { job, max_bid: b } if *job == outcome.job_id && *b == max_bid),
        "first event must be Opened with the escrowed B, got {:?}",
        events[0],
    );
    // Settled total is bounded by the escrowed B; the split is winner base + φ
    // platform fee + κ commission per agreeing non-winner. Under STRICT fee
    // enforcement the winner is paid exactly its derived `base` (not the leftover),
    // so integer-floor rounding may leave a few nanoton that refund to the
    // requester — total is ≤ B and within a nanoton-scale epsilon of B.
    match &events[1] {
        SettlementEvent::Settled { job, total } => {
            assert_eq!(*job, outcome.job_id);
            assert!(
                *total <= max_bid,
                "settle total {total} must not exceed escrow {max_bid}"
            );
            assert!(
                *total >= max_bid - 16,
                "the derived split spends ~all of B (only an integer-floor remainder \
                 of a few nanoton refunds); got {total} vs B {max_bid}"
            );
        }
        other => panic!("second event must be Settled, got {other:?}"),
    }

    // The settled paid job anchored exactly one record, which proves inclusion.
    assert_eq!(anchor.len(), 1, "settled paid job appends one JobRecord");
    let proof = anchor
        .prove_inclusion(&outcome.job_id)
        .expect("anchored record proves inclusion");
    let root = anchor.epoch_root();
    assert!(merkle::verify_inclusion(&root, &proof));

    // Reputation still updated (scoring is independent of settlement).
    assert!(st.reputation(&winner, now_ts()).is_some());
}

/// A settlement that records the FULL `SettlementOutcome` each settle carries
/// (winner + every participation commission + fee), so a test can assert the
/// multi-participant payout split the coordinator actually directs at the escrow.
struct CapturingSettlement {
    opened: std::sync::Mutex<Vec<Amount>>,
    settled: std::sync::Mutex<Vec<(Amount, SettlementOutcome)>>,
}
impl CapturingSettlement {
    fn new() -> Self {
        Self {
            opened: std::sync::Mutex::new(Vec::new()),
            settled: std::sync::Mutex::new(Vec::new()),
        }
    }
}
impl Settlement for CapturingSettlement {
    fn open_escrow(&self, job: &JobId, max_bid: Amount) -> Result<EscrowHandle, SettleError> {
        self.opened.lock().unwrap().push(max_bid);
        Ok(EscrowHandle {
            job: job.clone(),
            address: wallet(0xEE),
            max_bid,
        })
    }
    fn settle(&self, h: &EscrowHandle, outcome: &SettlementOutcome) -> Result<(), SettleError> {
        if outcome.total() > h.max_bid {
            return Err(SettleError::PayoutExceedsEscrow {
                payout: outcome.total(),
                escrow: h.max_bid,
            });
        }
        self.settled
            .lock()
            .unwrap()
            .push((h.max_bid, outcome.clone()));
        Ok(())
    }
    fn refund(&self, _: &EscrowHandle) -> Result<(), SettleError> {
        Ok(())
    }
    fn is_onchain(&self) -> bool {
        true
    }
}

/// A 3-replica PAID job: the winner plus TWO agreeing non-winners. The
/// coordinator must settle a payout split that pays the winner (base + bonus),
/// a fixed participation commission to EACH of the two agreeing runners, and the
/// platform fee — all bounded by the escrowed `B` (winner takes the remainder so
/// the total spends exactly `B`). This is the multi-participant commission path
/// the on-chain `participants` dict carries.
#[tokio::test]
async fn paid_job_settles_two_agreeing_participant_commissions() {
    let w1 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let w2 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let w3 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let st = store();
    let cfg = paid_cfg(3, 3);

    let reg = Arc::new(InMemoryStakeRegistry::new(0, 0, 0, 100_000 * TON));
    for w in [&w1, &w2, &w3] {
        reg.set_stake(&w.node_id, 1_000 * TON);
    }
    let settlement = Arc::new(CapturingSettlement::new());
    let anchor = Arc::new(InMemoryRecordAnchor::new());

    let coord = coordinator(
        &[&w1, &w2, &w3],
        cfg.clone(),
        st.clone() as Arc<dyn TrustStore>,
    )
    .await
    .with_stake_registry(reg)
    .with_settlement(settlement.clone())
    .with_record_anchor(anchor.clone());

    let outcome = coord
        .run_query("SELECT 1", QueryOverrides::default())
        .await
        .unwrap();
    assert!(outcome.verified);
    let winner = outcome.winner.clone().unwrap();
    let agreeing: Vec<NodeId> = outcome
        .participants
        .iter()
        .filter(|p| **p != winner)
        .cloned()
        .collect();
    assert_eq!(
        agreeing.len(),
        2,
        "two agreeing non-winners beside the winner"
    );

    let max_bid = 100 * TON;
    let opened = settlement.opened.lock().unwrap().clone();
    assert_eq!(
        opened,
        vec![max_bid],
        "escrow opened once for B before dispatch"
    );

    let settled = settlement.settled.lock().unwrap().clone();
    assert_eq!(settled.len(), 1, "settled exactly once");
    let (b, split) = &settled[0];
    assert_eq!(*b, max_bid);
    // Two participation commissions, each the same fixed κ·B, both > 0.
    assert_eq!(
        split.participants.len(),
        2,
        "two participant commissions in the split"
    );
    let c0 = split.participants[0].amount;
    let c1 = split.participants[1].amount;
    assert!(
        c0 > 0 && c0 == c1,
        "each agreeing non-winner earns the same fixed commission"
    );
    // Distinct payout wallets for the two participants.
    assert_ne!(split.participants[0].to, split.participants[1].to);
    // Winner takes the remainder; the whole split is bounded by B and spends it.
    assert!(
        split.total() <= max_bid,
        "payout split must not exceed the escrow B"
    );
    assert_eq!(
        split.total(),
        max_bid,
        "winner takes the remainder ⇒ total spends all of B"
    );
    assert_eq!(split.winner.amount, max_bid - split.platform_fee - c0 - c1);

    // The settled paid job anchored exactly one record.
    assert_eq!(anchor.len(), 1);
}

/// A FREE job, even with a settlement that PANICS on any call wired into the
/// coordinator, must complete the full grid path WITHOUT engaging the rail.
#[tokio::test]
async fn free_job_never_engages_coordinator_settlement() {
    let a = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let b = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let st = store();

    // Default economics ⇒ disabled ⇒ every job is free.
    let cfg = base_cfg(2, 2);
    assert!(!cfg.economics.enabled);
    let anchor = Arc::new(InMemoryRecordAnchor::new());
    let coord = coordinator(&[&a, &b], cfg, st as Arc<dyn TrustStore>)
        .await
        .with_settlement(Arc::new(PanicSettlement)) // panics if ever touched
        .with_record_anchor(anchor.clone());

    // If the coordinator engaged the rail on this free job, PanicSettlement would
    // panic and fail the test. It completes cleanly instead.
    let outcome = coord
        .run_query("SELECT 1", QueryOverrides::default())
        .await
        .unwrap();
    assert!(outcome.verified);
    assert_eq!(anchor.len(), 0, "free job anchors nothing");
}

// --------------------------------------------------------------------------
// GlobalParams sync wiring: overlay applied, escrow terms carry the synced
// version, anchored record stamped with it (BLOCKCHAIN_ECONOMICS §12).
// --------------------------------------------------------------------------

/// A `ParamsSource` returning a fixed on-chain policy (no network).
struct FixedParamsSource(OnchainPolicy);
impl ParamsSource for FixedParamsSource {
    fn read_policy(&self) -> Result<OnchainPolicy, SettleError> {
        Ok(self.0)
    }
}

/// A settlement that records the per-job terms (`expected_hash`, `params_version`)
/// the coordinator binds via the open-escrow-per-job path, plus the settle split.
struct TermsRecordingSettlement {
    opened: std::sync::Mutex<Vec<(Amount, Hash32, u32, Vec<WalletAddress>)>>,
    /// The chain-authoritative fee recipient passed into each open (the admin
    /// treasury the coordinator sourced from the synced GlobalParams policy).
    opened_fee_recipients: std::sync::Mutex<Vec<Option<WalletAddress>>>,
    settled: std::sync::Mutex<Vec<(Amount, SettlementOutcome)>>,
}
impl TermsRecordingSettlement {
    fn new() -> Self {
        Self {
            opened: std::sync::Mutex::new(Vec::new()),
            opened_fee_recipients: std::sync::Mutex::new(Vec::new()),
            settled: std::sync::Mutex::new(Vec::new()),
        }
    }
}
impl Settlement for TermsRecordingSettlement {
    fn open_escrow(&self, job: &JobId, max_bid: Amount) -> Result<EscrowHandle, SettleError> {
        // The coordinator must use the terms-aware entry; record a sentinel hash
        // so a regression (calling the termless path) is caught.
        self.opened
            .lock()
            .unwrap()
            .push((max_bid, [0xFFu8; 32], u32::MAX, Vec::new()));
        Ok(EscrowHandle {
            job: job.clone(),
            address: wallet(0xEE),
            max_bid,
        })
    }
    fn open_escrow_with_terms(
        &self,
        job: &JobId,
        max_bid: Amount,
        expected_hash: &Hash32,
        params_version: u32,
        candidates: &[WalletAddress],
        fee_recipient: Option<WalletAddress>,
    ) -> Result<EscrowHandle, SettleError> {
        self.opened
            .lock()
            .unwrap()
            .push((max_bid, *expected_hash, params_version, candidates.to_vec()));
        self.opened_fee_recipients.lock().unwrap().push(fee_recipient);
        Ok(EscrowHandle {
            job: job.clone(),
            address: wallet(0xEE),
            max_bid,
        })
    }
    fn settle(&self, h: &EscrowHandle, outcome: &SettlementOutcome) -> Result<(), SettleError> {
        if outcome.total() > h.max_bid {
            return Err(SettleError::PayoutExceedsEscrow {
                payout: outcome.total(),
                escrow: h.max_bid,
            });
        }
        self.settled
            .lock()
            .unwrap()
            .push((h.max_bid, outcome.clone()));
        Ok(())
    }
    fn refund(&self, _: &EscrowHandle) -> Result<(), SettleError> {
        Ok(())
    }
    fn is_onchain(&self) -> bool {
        true
    }
}

/// A record anchor that retains the full `JobRecord`s appended (to assert the
/// stamped `params_version`).
struct CapturingAnchor {
    records: std::sync::Mutex<Vec<JobRecord>>,
}
impl CapturingAnchor {
    fn new() -> Self {
        Self {
            records: std::sync::Mutex::new(Vec::new()),
        }
    }
}
impl RecordAnchor for CapturingAnchor {
    fn append(&self, record: &JobRecord) {
        self.records.lock().unwrap().push(record.clone());
    }
    fn epoch_root(&self) -> Hash32 {
        [0u8; 32]
    }
    fn prove_inclusion(&self, _job: &JobId) -> Option<InclusionProof> {
        None
    }
}

/// A synced on-chain policy must (1) be overlaid onto the PAID job's live config
/// (here a 10% platform fee, distinct from the 15% config default), (2) have its
/// version bound into the per-job escrow terms alongside the HTLC lock (the agreed
/// quorum hash), and (3) be stamped into the anchored `JobRecord` — proving the
/// startup sync genuinely drives fees + the params binding on the live path.
#[tokio::test]
async fn paid_job_syncs_params_overlays_config_and_binds_version() {
    let w1 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let w2 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let st = store();
    let cfg = paid_cfg(2, 2);
    // Sanity: the config default fee is the canonical 15% (so a 10% on-chain
    // overlay is visibly DIFFERENT from the default).
    assert!((cfg.economics.fees.platform_fee_pct - 0.15).abs() < 1e-9);

    let reg = Arc::new(InMemoryStakeRegistry::new(0, 0, 0, 100_000 * TON));
    reg.set_stake(&w1.node_id, 1_000 * TON);
    reg.set_stake(&w2.node_id, 1_000 * TON);
    let settlement = Arc::new(TermsRecordingSettlement::new());
    let anchor = Arc::new(CapturingAnchor::new());

    // On-chain policy: version 9, 10% platform fee, 0% participation commission.
    // The authoritative fee recipient matches the (zero) treasury in `paid_cfg`'s
    // `economics.fee_recipient`, so the coordinator's chain-vs-local cross-check
    // agrees and the job settles.
    let policy = OnchainPolicy {
        version: 9,
        platform_fee_bps: 1_000,
        fee_recipient: WalletAddress::new(0, [0u8; 32]),
        participation_commission_bps: 0,
        slash_failed_commitment_bps: 1_000,
        attempt_deadline_ms: 60_000,
        progress_interval_ms: 2_000,
        progress_stall_mult: 5,
    };

    let coord = coordinator(&[&w1, &w2], cfg, st.clone() as Arc<dyn TrustStore>)
        .await
        .with_stake_registry(reg)
        .with_settlement(settlement.clone())
        .with_record_anchor(anchor.clone())
        .with_params_source(Arc::new(FixedParamsSource(policy)));

    // Startup sync: read the on-chain policy into the cache (version + overlay).
    let synced = coord.sync_params_once().expect("sync reads the policy");
    assert_eq!(synced.version, 9);
    assert_eq!(coord.current_params_version(), 9);

    let outcome = coord
        .run_query("SELECT 1", QueryOverrides::default())
        .await
        .unwrap();
    assert!(outcome.verified);
    let agreed = outcome
        .agreed_hash
        .clone()
        .expect("quorum agreed on a hash");
    let max_bid = 100 * TON;

    // (2) The per-job escrow terms carry the synced version + the HTLC lock = the
    // agreed quorum result hash. The terms-aware path was used (not the sentinel).
    let opened = settlement.opened.lock().unwrap().clone();
    assert_eq!(opened.len(), 1, "escrow opened once after the quorum hash");
    let (b, expected_hash, version, _cands) = &opened[0];
    assert_eq!(*b, max_bid);
    assert_eq!(*version, 9, "escrow terms bind the SYNCED params version");
    assert_eq!(
        *expected_hash,
        *blake3::hash(agreed.as_bytes()).as_bytes(),
        "HTLC lock = quorum hash"
    );
    // The coordinator sourced the fee recipient (admin treasury) from the SYNCED
    // on-chain GlobalParams.fee_recipient and passed it into the open — not local config.
    let opened_fr = settlement.opened_fee_recipients.lock().unwrap().clone();
    assert_eq!(
        opened_fr[0],
        Some(WalletAddress::new(0, [0u8; 32])),
        "escrow treasury sourced from on-chain GlobalParams.fee_recipient"
    );

    // (1) The 10% on-chain fee overlaid the 15% config default in the settle split.
    // Under STRICT fee enforcement the platform fee is EXACTLY φ of the winner's
    // settled price (`base`), not of the whole escrow B — so assert it equals 10%
    // of the winner base the coordinator derived, and the whole split fits B.
    let settled = settlement.settled.lock().unwrap().clone();
    assert_eq!(settled.len(), 1);
    let (sb, split) = &settled[0];
    assert_eq!(*sb, max_bid);
    assert_eq!(
        split.platform_fee,
        split.winner.amount / 10,
        "synced 10% fee = exactly φ of the winner base (overlaid the 15% default)"
    );
    assert!(
        split.total() <= max_bid,
        "the full split (winner + fee + commissions) fits the locked escrow B"
    );
    assert!(
        split.participants.iter().all(|p| p.amount == 0),
        "0% participation commission synced"
    );

    // (3) The anchored record is stamped with the synced version.
    let records = anchor.records.lock().unwrap().clone();
    assert_eq!(records.len(), 1, "settled paid job anchors one record");
    assert_eq!(
        records[0].params_version, 9,
        "anchored record stamped with synced version"
    );
}

/// FEE-RECIPIENT enforcement (honest-party rejection): when the LOCAL
/// `economics.fee_recipient` disagrees with the authoritative on-chain
/// `GlobalParams.fee_recipient`, the coordinator REFUSES to open/settle the escrow
/// — the admin treasury cannot be silently overridden by local config. We assert
/// the job still runs/verifies off-chain but NO escrow is opened or settled.
#[tokio::test]
async fn paid_job_rejects_when_local_fee_recipient_disagrees_with_chain() {
    let w1 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let w2 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let st = store();
    // paid_cfg sets economics.fee_recipient = "0:0000…0" (the zero address).
    let cfg = paid_cfg(2, 2);

    // On-chain GlobalParams.fee_recipient is a DIFFERENT (real) treasury.
    let chain_treasury = WalletAddress::new(0, [0x7Eu8; 32]);
    let policy = OnchainPolicy {
        version: 5,
        platform_fee_bps: 1_500,
        fee_recipient: chain_treasury,
        participation_commission_bps: 500,
        slash_failed_commitment_bps: 1_000,
        attempt_deadline_ms: 60_000,
        progress_interval_ms: 2_000,
        progress_stall_mult: 5,
    };
    let settlement = Arc::new(TermsRecordingSettlement::new());
    let anchor = Arc::new(InMemoryRecordAnchor::new());

    let coord = coordinator(&[&w1, &w2], cfg, st.clone() as Arc<dyn TrustStore>)
        .await
        .with_settlement(settlement.clone())
        .with_record_anchor(anchor.clone())
        .with_params_source(Arc::new(FixedParamsSource(policy)));
    coord.sync_params_once().expect("sync reads the policy");
    assert_eq!(coord.current_fee_recipient(), Some(chain_treasury));

    let outcome = coord
        .run_query("SELECT 1", QueryOverrides::default())
        .await
        .unwrap();
    // The job still RUNS and verifies off-chain — only the on-chain settlement is
    // refused (the platform-fee recipient would be wrong).
    assert!(outcome.verified);

    assert!(
        settlement.opened.lock().unwrap().is_empty(),
        "no escrow opened when the local treasury disagrees with the chain recipient"
    );
    assert!(
        settlement.settled.lock().unwrap().is_empty(),
        "nothing settled on a fee-recipient mismatch"
    );
    assert_eq!(anchor.len(), 0, "no record anchored when settlement is refused");
}

/// FREE-NODE POLICY: a paid job whose WINNER is a free (walletless) node settles
/// with the winner paid 0 (the quoted `base` refunds to the requester), yet the
/// platform STILL collects exactly φ·base. Free agreeing verifiers earn nothing.
/// The on-chain fee formula is keyed on the quoted `base`, so it is non-zero even
/// though the winner payout is zero.
#[tokio::test]
async fn paid_job_free_winner_refunds_base_but_platform_still_collects_fee() {
    let w1 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let w2 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let st = store();
    let cfg = paid_cfg(2, 2); // φ = 15%, κ = 5% (config defaults)
    let settlement = Arc::new(TermsRecordingSettlement::new());
    let anchor = Arc::new(InMemoryRecordAnchor::new());

    // Every node is FREE (no wallet binding) — so whoever wins is a free winner and
    // any agreeing verifier is also walletless (earns no commission).
    let coord = coordinator(&[&w1, &w2], cfg, st.clone() as Arc<dyn TrustStore>)
        .await
        .with_settlement(settlement.clone())
        .with_record_anchor(anchor.clone())
        .with_wallet_binding(Arc::new(|_: &NodeId| None));

    let outcome = coord
        .run_query("SELECT 1", QueryOverrides::default())
        .await
        .unwrap();
    assert!(outcome.verified, "free nodes can fully run a paid job");

    let max_bid = 100 * TON;
    let settled = settlement.settled.lock().unwrap().clone();
    assert_eq!(settled.len(), 1, "settled exactly once");
    let (b, split) = &settled[0];
    assert_eq!(*b, max_bid);
    // The free winner is paid NOTHING (its base will refund to the requester).
    assert_eq!(split.winner.amount, 0, "free winner earns nothing");
    // The fee base (quoted price) is non-zero...
    assert!(split.base > 0, "the quoted base is still set for a free winner");
    // ...and the platform STILL collects exactly φ·base (15% of the base), proving
    // the fee is decoupled from the (zero) winner payout.
    assert_eq!(
        split.platform_fee,
        split.base * 1500 / 10_000,
        "platform collects exactly 15% of the quoted base even with a free winner"
    );
    // Free verifiers earn nothing (no wallet) — no commission legs.
    assert!(
        split.participants.iter().all(|p| p.amount == 0),
        "free verifiers earn no commission"
    );
    // All-or-nothing: the actual outflow still fits the locked escrow B.
    assert!(split.total() <= max_bid);
}

/// P0-2: the coordinator threads a requester-committed candidate payout set into
/// the per-job escrow at open (`open_escrow_with_terms`) that is EXACTLY the payee
/// set the settle presents (winner ∪ agreeing non-winners), so the on-chain
/// `candidatesCommitment(candidates) == terms.candidatesHash` check passes. We
/// assert the committed set equals the settle outcome's payees (as a set).
#[tokio::test]
async fn paid_job_commits_candidate_set_matching_settle_payees() {
    let w1 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let w2 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let w3 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let st = store();
    let cfg = paid_cfg(3, 3);

    let reg = Arc::new(InMemoryStakeRegistry::new(0, 0, 0, 100_000 * TON));
    for w in [&w1, &w2, &w3] {
        reg.set_stake(&w.node_id, 1_000 * TON);
    }
    let settlement = Arc::new(TermsRecordingSettlement::new());
    let anchor = Arc::new(InMemoryRecordAnchor::new());

    let coord = coordinator(&[&w1, &w2, &w3], cfg, st.clone() as Arc<dyn TrustStore>)
        .await
        .with_stake_registry(reg)
        .with_settlement(settlement.clone())
        .with_record_anchor(anchor.clone());

    let outcome = coord
        .run_query("SELECT 1", QueryOverrides::default())
        .await
        .unwrap();
    assert!(outcome.verified);

    let opened = settlement.opened.lock().unwrap().clone();
    let settled = settlement.settled.lock().unwrap().clone();
    assert_eq!(opened.len(), 1, "escrow opened once via the terms path");
    assert_eq!(settled.len(), 1, "settled once");
    let (_b, _hash, _ver, candidates) = &opened[0];
    let (_sb, split) = &settled[0];

    // The committed candidate set must equal the payees the settle presents.
    let mut committed = candidates.clone();
    committed.sort_by_key(|w| w.hash);
    let mut payees = vec![split.winner.to];
    for p in &split.participants {
        if !payees.contains(&p.to) {
            payees.push(p.to);
        }
    }
    payees.sort_by_key(|w| w.hash);
    assert_eq!(
        committed, payees,
        "open candidate commitment must equal the settle payees"
    );
    // winner + two agreeing non-winners ⇒ three distinct payout wallets committed.
    assert_eq!(committed.len(), 3, "winner + 2 agreeing participants");
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
        let coord = coordinator(
            &[&staked, &unstaked],
            paid_cfg(1, 1),
            st.clone() as Arc<dyn TrustStore>,
        )
        .await
        .with_stake_registry(reg.clone());
        let outcome = coord
            .run_query("SELECT 1", QueryOverrides::default())
            .await
            .unwrap();
        assert_eq!(
            outcome.winner.as_ref(),
            Some(&staked.node_id),
            "the staked worker must out-rank the unstaked peer on the paid grid",
        );
    }
}

/// `require_staked_hosts` is a hard candidate gate: with it on, only bonded hosts
/// are eligible, so a single-replica job routes exclusively to the staked worker
/// even when an unstaked peer is also discoverable.
#[tokio::test]
async fn require_staked_hosts_gate_excludes_unstaked_candidates() {
    let staked = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let unstaked = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let st = store();

    let reg = Arc::new(InMemoryStakeRegistry::new(0, 0, 0, 100_000 * TON));
    reg.set_stake(&staked.node_id, 1_000 * TON);
    // `unstaked` intentionally has zero stake.

    let coord = coordinator(
        &[&staked, &unstaked],
        base_cfg(1, 1),
        st.clone() as Arc<dyn TrustStore>,
    )
    .await
    .with_stake_registry(reg);

    for _ in 0..5 {
        let ov = QueryOverrides {
            require_staked_hosts: Some(true),
            ..Default::default()
        };
        let outcome = coord.run_query("SELECT 1", ov).await.unwrap();
        assert_eq!(
            outcome.winner.as_ref(),
            Some(&staked.node_id),
            "only the bonded host may be selected under the staked-hosts gate",
        );
    }
}

/// Fail-closed: with the staked-hosts gate on and NO stake registry wired, no
/// candidate can be proven bonded, so the query surfaces `NoCandidates` rather
/// than silently routing to unbonded hosts.
#[tokio::test]
async fn require_staked_hosts_without_registry_fails_closed() {
    let a = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let b = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let st = store();

    // No `.with_stake_registry(...)` wired.
    let coord = coordinator(&[&a, &b], base_cfg(1, 1), st as Arc<dyn TrustStore>).await;

    let ov = QueryOverrides {
        require_staked_hosts: Some(true),
        ..Default::default()
    };
    let err = coord.run_query("SELECT 1", ov).await.unwrap_err();
    assert!(
        matches!(err, p2p_node::CoordinatorError::NoCandidates),
        "staked-hosts gate with no registry must fail closed, got {err:?}",
    );
}

/// Anti-spoof: a worker that SELF-REPORTS L2 with no verifiable evidence must be
/// treated as L0 (no `AttestationVerifier` wired ⇒ fail closed), so an L2
/// (sensitive-tier) gate excludes it — selection no longer trusts the raw
/// self-reported attestation integer.
#[tokio::test]
async fn spoofed_attestation_level_is_downgraded_and_excluded() {
    let fake_l2 = p2p_proto::Attestation {
        level: p2p_proto::AttestationLevel::L2,
        evidence: Vec::new(),
        measurement: Some("claimed-but-unproven-enclave".into()),
    };
    let w = spawn_worker_att(Arc::new(MockEngine::deterministic()), fake_l2).await;
    let st = store();
    let coord = coordinator(&[&w], base_cfg(1, 1), st as Arc<dyn TrustStore>).await;

    let ov = QueryOverrides {
        min_attestation: Some("L2".into()),
        ..Default::default()
    };
    let err = coord.run_query("SELECT 1", ov).await.unwrap_err();
    assert!(
        matches!(
            err,
            p2p_node::CoordinatorError::InsufficientWorkers { .. }
                | p2p_node::CoordinatorError::NoCandidates
        ),
        "a spoofed L2 claim must be downgraded to L0 and excluded by the L2 gate, got {err:?}",
    );

    // Control: with NO attestation floor (default L0), the same host is admitted —
    // proving the exclusion was the attestation gate, not a blanket refusal.
    let outcome = coord
        .run_query("SELECT 1", QueryOverrides::default())
        .await
        .unwrap();
    assert!(outcome.verified);
}

/// Error propagation: a consensus-infeasible failure must carry the REAL
/// underlying engine error (e.g. the DuckDB `Binder Error: …` text) into the
/// `Infeasible` reason — not just the generic "N/N providers reported infeasible"
/// classification — so the requester can see WHAT was wrong with the query.
#[tokio::test]
async fn infeasible_reason_carries_real_engine_error() {
    let msg = "Binder Error: Referenced column \"alireza\" not found";
    let a = spawn_worker(Arc::new(MockEngine::failing(msg))).await;
    let b = spawn_worker(Arc::new(MockEngine::failing(msg))).await;
    let c = spawn_worker(Arc::new(MockEngine::failing(msg))).await;
    let st = store();
    let coord = coordinator(&[&a, &b, &c], base_cfg(3, 2), st as Arc<dyn TrustStore>).await;

    let err = coord
        .run_query("SELECT 45, \"alireza\"", QueryOverrides::default())
        .await
        .unwrap_err();
    match err {
        p2p_node::CoordinatorError::Infeasible { reason } => {
            // The generic classification is still present (job-fault distinction)…
            assert!(
                reason.contains("infeasible"),
                "should keep the job-fault classification: {reason}"
            );
            // …AND it now carries the real binder error, cleaned of wrappers.
            assert!(
                reason.contains("Binder Error: Referenced column \"alireza\" not found"),
                "reason must include the real engine error: {reason}"
            );
            // The internal wrapper prefixes are stripped.
            assert!(
                !reason.contains("exec error:") && !reason.contains("execution failed:"),
                "wrapper prefixes must be stripped: {reason}"
            );
        }
        other => panic!("expected Infeasible, got {other:?}"),
    }
}

/// Attestation honor-path (§7.2/§9.3): a host wired with a (software) attestor
/// emits per-offer, nonce-bound L2 evidence; a requester with the matching
/// `AttestationVerifier` HONORS the L2 level and admits it under an L2 gate.
/// Without the verifier, even valid evidence is ignored (downgraded to L0, fail
/// closed). Real TPM/TEE L1/L2 plugs in behind the same `Attestor`/`Verifier`.
#[tokio::test]
async fn attestation_honor_path_admits_verified_l2_host() {
    use ed25519_dalek::SigningKey;
    use p2p_proto::AttestationLevel;
    use p2p_trust::{AllowlistVerifier, MockAttestor};
    use rand::rngs::OsRng;

    let authority = SigningKey::generate(&mut OsRng);
    let authority_pub = authority.verifying_key().to_bytes();
    let bound = [7u8; 32];
    let attestor = Arc::new(MockAttestor::new(
        authority,
        "duckdb-enclave-v1",
        AttestationLevel::L2,
    ));
    let w = spawn_worker_attestor(Arc::new(MockEngine::deterministic()), attestor, bound).await;
    let st = store();

    let l2 = || QueryOverrides {
        min_attestation: Some("L2".into()),
        ..Default::default()
    };

    // No verifier wired ⇒ even valid evidence is not trusted above L0 → the L2
    // gate excludes the host (fail closed).
    {
        let coord = coordinator(&[&w], base_cfg(1, 1), st.clone() as Arc<dyn TrustStore>).await;
        let err = coord.run_query("SELECT 1", l2()).await.unwrap_err();
        assert!(
            matches!(
                err,
                p2p_node::CoordinatorError::InsufficientWorkers { .. }
                    | p2p_node::CoordinatorError::NoCandidates
            ),
            "no verifier ⇒ L2 claim downgraded, got {err:?}",
        );
    }

    // With the matching verifier, the per-offer nonce-bound evidence verifies and
    // the L2 level is honored ⇒ the host is admitted and wins.
    let verifier = Arc::new(AllowlistVerifier::new(
        authority_pub,
        ["duckdb-enclave-v1".to_string()],
        AttestationLevel::L0,
    ));
    let coord = coordinator(&[&w], base_cfg(1, 1), st as Arc<dyn TrustStore>)
        .await
        .with_attestation_verifier(verifier);
    let outcome = coord.run_query("SELECT 1", l2()).await.unwrap();
    assert!(outcome.verified);
    assert_eq!(outcome.winner.as_ref(), Some(&w.node_id));
}

/// Regression (console-server demo): with the `AttestationVerifier` wired and
/// L1/L2 hosts presenting real per-offer evidence (via `MockAttestor`), Internal
/// (L1, quorum 2) AND Sensitive (L2, quorum 4) jobs succeed — they MUST NOT all
/// collapse to `InsufficientWorkers` the way they did once the gate became
/// fail-closed but the demo forgot to wire the attestor/verifier. Mirrors the
/// demo's "Sensitive succeeds at quorum ≤ 4" with its 4 L2 hosts.
#[tokio::test]
async fn attestation_gate_admits_internal_and_sensitive_with_verifier() {
    use ed25519_dalek::SigningKey;
    use p2p_proto::AttestationLevel;
    use p2p_trust::{AllowlistVerifier, MockAttestor};
    use rand::rngs::OsRng;

    let authority = SigningKey::generate(&mut OsRng);
    let authority_pub = authority.verifying_key().to_bytes();
    let bound = [0xBD; 32];

    // 2 L1 hosts + 4 L2 hosts, each emitting real nonce-bound evidence.
    let mut hosts = Vec::new();
    for (level, meas) in [
        (AttestationLevel::L1, "img-l1"),
        (AttestationLevel::L1, "img-l1"),
        (AttestationLevel::L2, "img-l2"),
        (AttestationLevel::L2, "img-l2"),
        (AttestationLevel::L2, "img-l2"),
        (AttestationLevel::L2, "img-l2"),
    ] {
        let attestor = Arc::new(MockAttestor::new(authority.clone(), meas, level));
        hosts.push(spawn_worker_attestor(Arc::new(MockEngine::deterministic()), attestor, bound).await);
    }
    let refs: Vec<&WorkerHandle> = hosts.iter().collect();
    let st = store();
    let verifier = Arc::new(AllowlistVerifier::new(
        authority_pub,
        ["img-l1".to_string(), "img-l2".to_string()],
        AttestationLevel::L0,
    ));
    let coord = coordinator(&refs, base_cfg(6, 1), st as Arc<dyn TrustStore>)
        .await
        .with_attestation_verifier(verifier);

    // Internal (L1) at quorum 2: every attested host (≥ L1) qualifies.
    let internal = QueryOverrides {
        quorum: Some(2),
        min_attestation: Some("L1".into()),
        ..Default::default()
    };
    let o = coord.run_query("SELECT 1", internal).await.unwrap();
    assert!(o.verified, "Internal must verify with attested L1+ hosts");
    assert!(o.agreement >= 2, "agreement {} < quorum 2", o.agreement);

    // Sensitive (L2) at quorum 4: only the 4 L2 hosts qualify, and that's enough.
    let sensitive = QueryOverrides {
        quorum: Some(4),
        min_attestation: Some("L2".into()),
        ..Default::default()
    };
    let o = coord.run_query("SELECT 1", sensitive).await.unwrap();
    assert!(o.verified, "Sensitive must verify with 4 attested L2 hosts");
    assert!(o.agreement >= 4, "agreement {} < quorum 4", o.agreement);
}

// --------------------------------------------------------------------------
// Request-scoping / routing constraints (§7.5) — coordinator selection
// --------------------------------------------------------------------------

/// Region pin is FAIL-CLOSED at the coordinator: TOFU candidates advertise no
/// region, so a region-pinned query can prove none are in-region and surfaces
/// `NoCandidates` (mirrors `require_staked_hosts_without_registry_fails_closed`).
#[tokio::test]
async fn region_pin_fails_closed_without_advertised_region() {
    let a = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let b = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let st = store();
    let coord = coordinator(&[&a, &b], base_cfg(1, 1), st as Arc<dyn TrustStore>).await;

    let ov = QueryOverrides {
        regions: vec!["eu".into()],
        ..Default::default()
    };
    let err = coord.run_query("SELECT 1", ov).await.unwrap_err();
    assert!(
        matches!(err, p2p_node::CoordinatorError::NoCandidates),
        "a region pin with no in-region host must fail closed, got {err:?}",
    );

    // Control: no region constraint ⇒ the same hosts serve the query.
    let outcome = coord
        .run_query("SELECT 1", QueryOverrides::default())
        .await
        .unwrap();
    assert!(outcome.verified);
}

/// Network targeting is enforced at WORKER admission end-to-end: default hosts
/// serve the implicit "default" partition, so a query targeting a different
/// partition is declined by every host (no eligible workers). Targeting the
/// host's own partition is served — proving it's the constraint, not a refusal.
#[tokio::test]
async fn network_target_excludes_hosts_in_other_partition() {
    let a = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let st = store();
    let coord = coordinator(&[&a], base_cfg(1, 1), st as Arc<dyn TrustStore>).await;

    let ov = QueryOverrides {
        network: Some("eu".into()),
        ..Default::default()
    };
    let err = coord.run_query("SELECT 1", ov).await.unwrap_err();
    assert!(
        matches!(
            err,
            p2p_node::CoordinatorError::InsufficientWorkers { .. }
                | p2p_node::CoordinatorError::NoCandidates
        ),
        "a default-partition host must decline an offer targeting another network, got {err:?}",
    );

    let ov_default = QueryOverrides {
        network: Some("default".into()),
        ..Default::default()
    };
    let outcome = coord.run_query("SELECT 1", ov_default).await.unwrap();
    assert!(outcome.verified);
}

/// Sealed/scoped credential delivery (§9.2): when a credential provider is
/// wired, the coordinator mints a per-job scoped credential into the `Dispatch`;
/// the worker accepts and executes it. With the MockEngine the credential rides
/// along unused, proving the live path is non-destabilizing.
#[tokio::test]
async fn credential_provider_attaches_without_breaking_query() {
    let w = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let st = store();
    let coord = coordinator(&[&w], base_cfg(1, 1), st as Arc<dyn TrustStore>)
        .await
        .with_credential_provider(Arc::new(p2p_node::FakeStsS3Provider::new("eu-west-1")));
    let outcome = coord
        .run_query("SELECT 1", QueryOverrides::default())
        .await
        .unwrap();
    assert!(outcome.verified);
}

/// Wallet-collateral history binding (G4): reputation is aggregated across every
/// node id bound to the same collateral wallet, so rotating the node key alone
/// (re-binding the new id to the same wallet) does NOT shed accumulated history.
#[tokio::test]
async fn wallet_binding_aggregates_reputation_across_rotated_keys() {
    use p2p_config::BindingStore;

    let veteran = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let rotated = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let st = store();

    // Build the veteran key's reputation via many verified jobs (shared store).
    {
        let coord = coordinator(&[&veteran], base_cfg(1, 1), st.clone() as Arc<dyn TrustStore>)
            .await;
        for _ in 0..15 {
            coord
                .run_query("SELECT 1", QueryOverrides::default())
                .await
                .unwrap();
        }
    }

    // A min-trust gate the FRESH rotated key can't clear on its own (bootstrap).
    let mut cfg = base_cfg(1, 1);
    cfg.trust.min_trust = 0.5;
    cfg.validate().unwrap();

    // Without a binding, the rotated key has no history ⇒ excluded.
    {
        let coord = coordinator(&[&rotated], cfg.clone(), st.clone() as Arc<dyn TrustStore>).await;
        let err = coord
            .run_query("SELECT 1", QueryOverrides::default())
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                p2p_node::CoordinatorError::InsufficientWorkers { .. }
                    | p2p_node::CoordinatorError::NoCandidates
            ),
            "fresh rotated key with no history must miss the gate, got {err:?}",
        );
    }

    // Bind BOTH keys to the same collateral wallet: the rotated key now inherits
    // the veteran's aggregated history and clears the same gate.
    let dir = tempfile::tempdir().unwrap();
    let bs = Arc::new(BindingStore::with_path(dir.path().join("bindings.toml")));
    bs.bind(veteran.node_id.as_str(), "EQ_collateral_wallet", "h", 1)
        .unwrap();
    bs.bind(rotated.node_id.as_str(), "EQ_collateral_wallet", "h", 2)
        .unwrap();
    let coord = coordinator(&[&rotated], cfg, st.clone() as Arc<dyn TrustStore>)
        .await
        .with_binding_store(bs);
    let outcome = coord
        .run_query("SELECT 1", QueryOverrides::default())
        .await
        .unwrap();
    assert_eq!(
        outcome.winner.as_ref(),
        Some(&rotated.node_id),
        "rotated key inherits wallet-bound history and clears the gate",
    );
}

// --------------------------------------------------------------------------
// Anti-cheat: newcomer trust ceiling + Sybil stake-floor gate (default-off)
// --------------------------------------------------------------------------

/// The newcomer trust ceiling caps a thin-history node's selection score: with a
/// low ceiling and a high observation threshold, a fresh node is capped below the
/// `min_trust` gate and excluded; with the ceiling off (1.0) the SAME fresh node
/// clears the gate. Isolates the ceiling (only that knob changes).
#[tokio::test]
async fn newcomer_trust_ceiling_caps_fresh_nodes() {
    let w = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let st = store();

    // Ceiling off (1.0): the fresh node's bootstrap score clears a tiny gate.
    {
        let mut cfg = base_cfg(1, 1);
        cfg.trust.min_trust = 0.01;
        cfg.economics.ranking.newcomer_trust_ceiling = 1.0;
        cfg.validate().unwrap();
        let coord = coordinator(&[&w], cfg, st.clone() as Arc<dyn TrustStore>).await;
        let outcome = coord
            .run_query("SELECT 1", QueryOverrides::default())
            .await
            .unwrap();
        assert!(outcome.verified, "uncapped fresh node clears the gate");
    }

    // Ceiling 0.0 with a high threshold: the same fresh node is capped to 0.0,
    // below the 0.01 gate, so it is excluded → no usable workers.
    {
        let mut cfg = base_cfg(1, 1);
        cfg.trust.min_trust = 0.01;
        cfg.economics.ranking.newcomer_trust_ceiling = 0.0;
        cfg.economics.ranking.newcomer_obs_threshold = 100;
        cfg.validate().unwrap();
        let coord = coordinator(&[&w], cfg, st.clone() as Arc<dyn TrustStore>).await;
        let err = coord
            .run_query("SELECT 1", QueryOverrides::default())
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                p2p_node::CoordinatorError::InsufficientWorkers { .. }
                    | p2p_node::CoordinatorError::NoCandidates
            ),
            "capped newcomer must be excluded, got {err:?}",
        );
    }
}

/// The Sybil stake-floor gate (`[sybil].min_stake`, whole TON) excludes a
/// candidate whose bonded stake is below the floor, even when a richer peer is
/// also discoverable — raising the cost of throwaway identities. Default 0 = off.
#[tokio::test]
async fn sybil_min_stake_gate_excludes_under_staked_candidates() {
    let rich = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let poor = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let st = store();
    let reg = Arc::new(InMemoryStakeRegistry::new(0, 0, 0, 100_000 * TON));
    reg.set_stake(&rich.node_id, 50 * TON); // clears the 10-TON floor
    reg.set_stake(&poor.node_id, 1 * TON); // below the floor

    let mut cfg = base_cfg(1, 1);
    cfg.sybil.min_stake = 10; // whole TON
    cfg.validate().unwrap();
    let coord = coordinator(&[&rich, &poor], cfg, st.clone() as Arc<dyn TrustStore>)
        .await
        .with_stake_registry(reg);

    for _ in 0..5 {
        let outcome = coord
            .run_query("SELECT 1", QueryOverrides::default())
            .await
            .unwrap();
        assert_eq!(
            outcome.winner.as_ref(),
            Some(&rich.node_id),
            "only the sufficiently-staked host may be selected under the Sybil floor",
        );
    }
}

/// Fail-closed: the Sybil stake-floor gate with NO stake registry wired admits
/// nobody (no candidate can be proven to clear the floor) → `NoCandidates`.
#[tokio::test]
async fn sybil_min_stake_without_registry_fails_closed() {
    let a = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let b = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let st = store();

    let mut cfg = base_cfg(1, 1);
    cfg.sybil.min_stake = 5;
    cfg.validate().unwrap();
    let coord = coordinator(&[&a, &b], cfg, st as Arc<dyn TrustStore>).await;

    let err = coord
        .run_query("SELECT 1", QueryOverrides::default())
        .await
        .unwrap_err();
    assert!(
        matches!(err, p2p_node::CoordinatorError::NoCandidates),
        "Sybil stake floor with no registry must fail closed, got {err:?}",
    );
}

// --------------------------------------------------------------------------
// Time-based (usage) pricing: metered settle + refund, hard cap-deadline
// abort (no slash), up-front underfunded rejection, perf prioritization.
// --------------------------------------------------------------------------

/// Spawn a worker that advertises METERED pricing (a per-second rate + the cap
/// multiplier) so its bids carry `rate_per_second`/`cap_seconds`. The engine can
/// be a delayed `MockEngine` to control measured processing latency.
async fn spawn_worker_metered(
    engine: Arc<dyn QueryEngine>,
    rate_per_second: u64,
    cap_multiplier: u64,
) -> WorkerHandle {
    let net = GridConfig::default().network;
    let transport =
        Arc::new(QuicTransport::bind(&net, &idcfg(), NodeIdentity::generate().unwrap()).unwrap());
    let mut cfg = GridConfig::default();
    cfg.economics.pricing.model = PricingModel::Metered;
    cfg.economics.pricing.rate_per_second = rate_per_second;
    cfg.economics.pricing.cap_multiplier = cap_multiplier;
    let admission = AdmissionController::new(&cfg.budget);
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

/// A paid grid config priced under the METERED model (per-second rate + 5× cap),
/// with `max_bid` (the requester's maxEscrow ceiling) in whole TON.
fn metered_cfg(
    replicas: usize,
    quorum: usize,
    rate_per_second: u64,
    cap_multiplier: u64,
    max_bid_ton: u64,
) -> GridConfig {
    let mut c = paid_cfg(replicas, quorum);
    c.economics.pricing.model = PricingModel::Metered;
    c.economics.pricing.rate_per_second = rate_per_second;
    c.economics.pricing.cap_multiplier = cap_multiplier;
    c.economics.pricing.max_bid = max_bid_ton;
    c.validate().unwrap();
    c
}

fn estimate_bytes(bytes: u64) -> WorkingSetEstimate {
    WorkingSetEstimate {
        scanned_uncompressed_bytes: bytes,
        estimated_rows: 0,
        scan_buffer_bytes: 0,
        group_by_bytes: 0,
        join_build_bytes: 0,
        sort_bytes: 0,
        peak_working_set_bytes: 0,
        estimated_runtime_ms: 0,
    }
}

/// Seed a worker's COUNTERPARTY-MEASURED perf aggregate + reputation by recording
/// `n` verified-`Correct` receipts with the given measured latency / workload
/// bytes (the same path real signed receipts take). Distinct job ids so each is
/// folded once (replay-deduped).
fn seed_perf(st: &InMemoryTrustStore, worker: &NodeId, n: usize, latency_ms: u64, bytes: u64) {
    for i in 0..n {
        let r = Receipt {
            job_id: JobId(format!("seed-{}-{i}", worker.as_str())),
            worker_id: worker.clone(),
            requester_id: NodeId("b3:seed-requester".into()),
            query_hash: QueryHash::compute("seed", "v"),
            result_hash: "h".into(),
            verdict: Verdict::Correct,
            latency_ms,
            ts: now_ts(),
            observed_input_bytes: bytes,
            observed_result_rows: 0,
            observed_result_bytes: 0,
            input_fingerprint: String::new(),
            requester_pubkey: String::new(),
            sig: String::new(),
        };
        st.record(&r);
        st.observe_capability(&r);
        st.observe_perf(&r);
    }
}

/// METERED settle: the coordinator sizes the escrow to the worst-case `cap_base`
/// (`rate × cap_seconds`), bills `base = rate × min(actual_seconds, cap_seconds)`,
/// applies the φ=15% / κ=5% split, and REFUNDS the unused over-reservation (the
/// settle total is far below the cap-sized escrow `B`). The 16 MiB/s cold-start
/// throughput × a controlled estimate makes `cap_seconds` deterministic.
#[tokio::test]
async fn metered_job_bills_actual_seconds_and_refunds_unused_escrow() {
    const TON: Amount = 1_000_000_000;
    let rate = TON as u64; // 1 TON / second
                           // ~16 GiB estimate ⇒ estimated_seconds = 1000, cap_seconds = 5000.
    let est_bytes = 16 * 1024 * 1024 * 1000u64;
    // A ~600ms execution ⇒ ceil(0.6s)=1 billed second (well under the 5000 cap).
    let engine = || Arc::new(MockEngine::deterministic().with_delay(Duration::from_millis(600)));
    let w1 = spawn_worker_metered(engine(), rate, 5).await;
    let w2 = spawn_worker_metered(engine(), rate, 5).await;
    let st = store();
    // maxEscrow comfortably covers the worst case (cap_base 5000 TON + fees).
    let cfg = metered_cfg(2, 2, rate, 5, 10_000);
    let settlement = Arc::new(CapturingSettlement::new());

    let coord = coordinator(&[&w1, &w2], cfg, st.clone() as Arc<dyn TrustStore>)
        .await
        .with_settlement(settlement.clone());

    let outcome = coord
        .run_query_planned(
            "SELECT 1",
            QueryOverrides::default(),
            Some(estimate_bytes(est_bytes)),
        )
        .await
        .unwrap();
    assert!(outcome.verified);

    // cap_seconds = ceil(16GiB / 16MiB/s) × 5 = 1000 × 5 = 5000; cap_base = rate×cap.
    let cap_base = rate as Amount * 5000;
    let expected_b = p2p_settlement::required_escrow_total(cap_base, 1, 1500, 500);

    let opened = settlement.opened.lock().unwrap().clone();
    assert_eq!(opened, vec![expected_b], "escrow sized to the worst-case cap_base");

    let settled = settlement.settled.lock().unwrap().clone();
    assert_eq!(settled.len(), 1);
    let (b, split) = &settled[0];
    assert_eq!(*b, expected_b);

    // base = rate × actual ≤ cap_base; a ~600ms job bills 1–2 whole seconds.
    let base = split.base;
    assert!(base > 0 && base % rate as Amount == 0, "base is a whole-second multiple of the rate: {base}");
    assert!(base <= cap_base, "base must not exceed the cap base");
    assert!(base <= 2 * rate as Amount, "≈1s job bills ~1 second, not the cap");
    // The winner has a wallet ⇒ paid exactly the metered base.
    assert_eq!(split.winner.amount, base);
    // Correct 15% / 5% split.
    assert_eq!(split.platform_fee, base * 1500 / 10_000);
    assert_eq!(split.participants.len(), 1);
    assert_eq!(split.participants[0].amount, base * 500 / 10_000);
    // The full split fits B AND leaves a large refund (cap over-reservation − actual).
    let total = split.total();
    assert!(total <= *b);
    assert!(*b - total > 0, "unused escrow (cap − actual) refunds to the requester");
    assert!(total < cap_base, "actual spend is far below the worst-case cap");
}

/// HARD cap deadline: a metered job whose execution exceeds `cap_seconds` is
/// ABORTED at the cap (no grace) and classified as a resource/job fault. With a
/// consensus of providers overrunning, the query is `Infeasible` — NO escrow is
/// opened/settled and NO stake is slashed (the data was simply too big for the
/// bid cap; the providers are blameless).
#[tokio::test]
async fn metered_overrun_past_cap_aborts_with_no_settle_and_no_slash() {
    const TON: Amount = 1_000_000_000;
    let rate = TON as u64;
    // Tiny estimate ⇒ estimated_seconds = 1, cap_multiplier 1 ⇒ cap_seconds = 1
    // (cap deadline = 1000ms). The engine takes 2500ms ⇒ a hard cap overrun.
    let engine = || Arc::new(MockEngine::deterministic().with_delay(Duration::from_millis(2500)));
    let w1 = spawn_worker_metered(engine(), rate, 1).await;
    let w2 = spawn_worker_metered(engine(), rate, 1).await;
    let st = store();
    let cfg = metered_cfg(2, 2, rate, 1, 1_000); // ample maxEscrow (not the point here)

    let reg = Arc::new(InMemoryStakeRegistry::new(0, 0, 0, 100_000 * TON));
    reg.set_stake(&w1.node_id, 1_000 * TON);
    reg.set_stake(&w2.node_id, 1_000 * TON);
    let settlement = Arc::new(CapturingSettlement::new());

    let coord = coordinator(&[&w1, &w2], cfg, st.clone() as Arc<dyn TrustStore>)
        .await
        .with_stake_registry(reg.clone())
        .with_settlement(settlement.clone());

    let err = coord
        .run_query_planned("SELECT 1", QueryOverrides::default(), Some(estimate_bytes(1_000)))
        .await
        .unwrap_err();
    assert!(
        matches!(err, p2p_node::CoordinatorError::Infeasible { .. }),
        "a consensus cap overrun is a job fault (Infeasible), got {err:?}",
    );

    // NO escrow opened/settled (the job never reached a verified quorum).
    assert!(settlement.opened.lock().unwrap().is_empty(), "no escrow on an aborted job");
    assert!(settlement.settled.lock().unwrap().is_empty(), "nothing settled on an aborted job");
    // NO slash: the cap overrun is a resource/job fault, never a provider penalty.
    assert_eq!(reg.stake_of(&w1.node_id), 1_000 * TON, "overrun must not slash w1");
    assert_eq!(reg.stake_of(&w2.node_id), 1_000 * TON, "overrun must not slash w2");
    // The overrunning nodes accrue no fault observation either.
    assert_eq!(st.observation_count(&w1.node_id), 0);
    assert_eq!(st.observation_count(&w2.node_id), 0);
}

/// UP-FRONT underfunded rejection: when the requester's maxEscrow cannot cover the
/// worst-case `cap_base + φ + κ·N` of the selected metered bids, the job is
/// REJECTED before any dispatch with the human-readable insufficient-escrow error
/// — and nothing is ever opened/settled.
#[tokio::test]
async fn metered_underfunded_job_is_rejected_up_front() {
    const TON: Amount = 1_000_000_000;
    let rate = TON as u64;
    let est_bytes = 16 * 1024 * 1024 * 1000u64; // ⇒ cap_seconds 5000 ⇒ cap_base 5000 TON
    let w1 = spawn_worker_metered(Arc::new(MockEngine::deterministic()), rate, 5).await;
    let w2 = spawn_worker_metered(Arc::new(MockEngine::deterministic()), rate, 5).await;
    let st = store();
    // maxEscrow only 100 TON — far short of cap_base (5000 TON) + fees.
    let cfg = metered_cfg(2, 2, rate, 5, 100);
    let settlement = Arc::new(CapturingSettlement::new());

    let coord = coordinator(&[&w1, &w2], cfg, st as Arc<dyn TrustStore>)
        .await
        .with_settlement(settlement.clone());

    let err = coord
        .run_query_planned(
            "SELECT 1",
            QueryOverrides::default(),
            Some(estimate_bytes(est_bytes)),
        )
        .await
        .unwrap_err();
    match err {
        p2p_node::CoordinatorError::InsufficientEscrow(msg) => {
            assert!(msg.contains("insufficient escrow"), "got: {msg}");
        }
        other => panic!("expected InsufficientEscrow, got {other:?}"),
    }
    // Rejected before any dispatch ⇒ nothing opened/settled.
    assert!(settlement.opened.lock().unwrap().is_empty());
    assert!(settlement.settled.lock().unwrap().is_empty());
}

/// PERFORMANCE PRIORITIZATION (latency): with equal reputation, the node whose
/// COUNTERPARTY-MEASURED commit latency is low (`perf_sample`) out-ranks a node
/// measured slow — so the faster honest node wins dispatch deterministically.
#[tokio::test]
async fn perf_prioritization_picks_measured_fast_over_slow() {
    let fast = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let slow = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let st = store();
    // Equal reputation (10 verified successes each); only the MEASURED latency
    // differs — fast 100ms vs slow 4900ms over the same 1 GiB workload.
    let gib = 1024 * 1024 * 1024u64;
    seed_perf(&st, &fast.node_id, 10, 100, gib);
    seed_perf(&st, &slow.node_id, 10, 4900, gib);

    for _ in 0..5 {
        let coord = coordinator(&[&fast, &slow], base_cfg(1, 1), st.clone() as Arc<dyn TrustStore>).await;
        let outcome = coord
            .run_query("SELECT 1", QueryOverrides::default())
            .await
            .unwrap();
        assert_eq!(
            outcome.winner.as_ref(),
            Some(&fast.node_id),
            "the measured-fast node must out-rank the measured-slow peer",
        );
    }
}

/// PERFORMANCE PRIORITIZATION (anti-game): both nodes self-report the SAME ETA in
/// their bids (each CLAIMS to be equally fast), yet the one COUNTERPARTY-MEASURED
/// to be slow is demoted — selection scores on `perf_sample` (receipt-driven),
/// never on the self-reported ETA. A node that over-claims speed but delivers
/// slow loses the race. Also exercises the throughput term (higher measured
/// bytes/second ⇒ higher rank).
#[tokio::test]
async fn perf_prioritization_demotes_node_measured_slow_despite_equal_self_report() {
    let honest = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let overclaimer = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let st = store();
    // Identical reputation. Same measured latency, but the honest node is measured
    // pushing far more bytes/second (higher throughput) than the over-claimer.
    seed_perf(&st, &honest.node_id, 10, 1000, 10 * 1024 * 1024 * 1024);
    seed_perf(&st, &overclaimer.node_id, 10, 1000, 64 * 1024 * 1024);
    // Both workers bid the SAME self-reported ETA (the offer carries no cost hint),
    // so the only differentiator is the counterparty-measured perf.
    for _ in 0..5 {
        let coord = coordinator(
            &[&honest, &overclaimer],
            base_cfg(1, 1),
            st.clone() as Arc<dyn TrustStore>,
        )
        .await;
        let outcome = coord
            .run_query("SELECT 1", QueryOverrides::default())
            .await
            .unwrap();
        assert_eq!(
            outcome.winner.as_ref(),
            Some(&honest.node_id),
            "measured throughput/latency must decide, not the self-reported ETA",
        );
    }
}
