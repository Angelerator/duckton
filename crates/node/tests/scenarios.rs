//! Comprehensive end-to-end SCENARIO suite (first-class deliverable).
//!
//! Each scenario is deterministic and self-contained, using in-process loopback
//! QUIC multi-node setups + the deterministic mock engine + the mock attestor /
//! local-fake storage where real hardware/cloud is unavailable. Sandbox-lockdown
//! scenarios that require the real engine live in `duckdb_engine.rs` behind the
//! `duckdb-engine` feature; key-release scenarios live in `storage.rs`.
//!
//! Scenario index (see test names): functional, hedging/trust, admission,
//! versioning, config, and resilience/churn.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use p2p_config::{
    BudgetConfig, DataClassCfg, GridConfig, IdentityConfig, PaymentPref, PinningMode,
    QueryOverrides, SettlementRail,
};
use p2p_node::{
    AdmissionController, CanaryAuditor, Candidate, Coordinator, Discovery, MembershipTable,
    MockEngine, QueryEngine, StaticDiscovery, Worker, WorkerParams,
};
use p2p_proto::{Attestation, AttestationLevel, NodeId, ResultSet, Value, Verdict, Version};
use p2p_settlement::InMemoryStakeRegistry;
use p2p_transport::{NodeIdentity, QuicTransport, Transport, VersionInfo};
use p2p_trust::sybil::pow_epoch;
use p2p_trust::{
    canonical_hash, mint_pow, now_ts, sign_capability_ad, CapabilityDraft, InMemoryTrustStore,
    TrustStore,
};

// --------------------------------------------------------------------------
// Harness
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

async fn spawn_worker_with_budget(
    engine: Arc<dyn QueryEngine>,
    budget: BudgetConfig,
) -> WorkerHandle {
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
    );
    let task = worker.spawn();
    WorkerHandle {
        node_id,
        addr,
        _transport: transport,
        _task: task,
    }
}

async fn spawn_worker(engine: Arc<dyn QueryEngine>) -> WorkerHandle {
    spawn_worker_with_budget(engine, GridConfig::default().budget).await
}

fn cfg(replicas: usize, quorum: usize) -> GridConfig {
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
// FUNCTIONAL
// --------------------------------------------------------------------------

#[tokio::test]
async fn scenario_two_node_result_matches_locally_computed() {
    let w = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let coord = coordinator(&[&w], cfg(1, 1), store()).await;
    let sql = "SELECT region, count(*) FROM 's3://bucket/events/*.parquet' GROUP BY region";

    let outcome = coord
        .run_query(sql, QueryOverrides::default())
        .await
        .unwrap();

    // Locally compute the expected result with the same deterministic engine.
    let expected = MockEngine::deterministic()
        .execute(
            sql,
            p2p_node::ExecLease {
                memory_bytes: 1 << 20,
                threads: 1,
            },
        )
        .await
        .unwrap();
    assert_eq!(outcome.result, expected);
    assert_eq!(
        outcome.agreed_hash.as_deref(),
        Some(canonical_hash(&expected).as_str())
    );
}

#[tokio::test]
async fn scenario_large_result_streaming_with_backpressure() {
    // ~200k rows forces many QUIC chunks; the streaming path must not error and
    // must reassemble exactly. QUIC flow control provides backpressure.
    const N: i64 = 200_000;
    let rows: Vec<Vec<Value>> = (0..N)
        .map(|i| vec![Value::Int(i), Value::Int(i * 2)])
        .collect();
    let rs = ResultSet::new(vec!["i".into(), "double".into()], rows);
    let mut fixtures = HashMap::new();
    fixtures.insert("SELECT big".to_string(), rs.clone());

    let w = spawn_worker(Arc::new(MockEngine::with_fixtures(fixtures))).await;
    let coord = coordinator(&[&w], cfg(1, 1), store()).await;

    let outcome = coord
        .run_query("SELECT big", QueryOverrides::default())
        .await
        .unwrap();
    assert_eq!(outcome.result.row_count(), N as usize);
    assert_eq!(outcome.result, rs);
}

#[tokio::test]
async fn scenario_large_result_parallel_streams_and_compression() {
    // The same large result, transferred over 4 parallel unidirectional streams
    // with zstd wire compression, must reassemble byte-identically. Exercises the
    // manifest + multi-stream + decompress path end-to-end.
    const N: i64 = 120_000;
    let rows: Vec<Vec<Value>> = (0..N)
        .map(|i| vec![Value::Int(i), Value::Text(format!("row-{}", i % 97))])
        .collect();
    let rs = ResultSet::new(vec!["i".into(), "label".into()], rows);
    let mut fixtures = HashMap::new();
    fixtures.insert("SELECT wide".to_string(), rs.clone());

    let w = spawn_worker(Arc::new(MockEngine::with_fixtures(fixtures))).await;

    let mut c = cfg(1, 1);
    c.transport.result.parallelism = 4;
    c.transport.result.parallel_min_bytes = 4096; // force fan-out for this payload
    c.transport.compression.algorithm = p2p_config::CompressionAlgo::Zstd;
    c.transport.compression.min_size_bytes = 4096;
    c.transport.quic.max_concurrent_uni_streams = 16;
    c.validate().unwrap();
    let coord = coordinator(&[&w], c, store()).await;

    let outcome = coord
        .run_query("SELECT wide", QueryOverrides::default())
        .await
        .unwrap();
    assert_eq!(outcome.result.row_count(), N as usize);
    assert_eq!(outcome.result, rs);
}

#[tokio::test]
async fn scenario_result_parallelism_overridable_per_call() {
    // Per-call override drives the fan-out without touching base config.
    const N: i64 = 80_000;
    let rows: Vec<Vec<Value>> = (0..N)
        .map(|i| vec![Value::Int(i), Value::Int(i * 3)])
        .collect();
    let rs = ResultSet::new(vec!["a".into(), "b".into()], rows);
    let mut fixtures = HashMap::new();
    fixtures.insert("SELECT par".to_string(), rs.clone());

    let w = spawn_worker(Arc::new(MockEngine::with_fixtures(fixtures))).await;
    let mut c = cfg(1, 1);
    c.transport.result.parallel_min_bytes = 4096;
    let coord = coordinator(&[&w], c, store()).await;

    let overrides = QueryOverrides {
        result_parallelism: Some(8),
        compression: Some(p2p_config::CompressionAlgo::Lz4),
        ..Default::default()
    };
    let outcome = coord.run_query("SELECT par", overrides).await.unwrap();
    assert_eq!(outcome.result, rs);
}

#[tokio::test]
async fn scenario_many_concurrent_jobs_across_workers() {
    let big_budget = BudgetConfig {
        memory_bytes: 64 * 1024 * 1024 * 1024,
        threads: 64,
        max_jobs: 64,
        per_job_memory_bytes: 64 * 1024 * 1024,
        per_job_threads: 1,
        data_classes: vec![DataClassCfg::Public],
    };
    let w1 =
        spawn_worker_with_budget(Arc::new(MockEngine::deterministic()), big_budget.clone()).await;
    let w2 =
        spawn_worker_with_budget(Arc::new(MockEngine::deterministic()), big_budget.clone()).await;
    let w3 = spawn_worker_with_budget(Arc::new(MockEngine::deterministic()), big_budget).await;
    // Generous timeouts: per-connection version handshakes are currently
    // accepted serially per endpoint, so a burst of fresh connections needs
    // headroom (see ARCHITECTURE.md "scalability refinements").
    let mut c = cfg(3, 2);
    c.scheduler.offer_timeout_ms = 8_000;
    c.scheduler.dispatch_timeout_ms = 15_000;
    let coord = Arc::new(coordinator(&[&w1, &w2, &w3], c, store()).await);

    let mut handles = Vec::new();
    for i in 0..10 {
        let c = Arc::clone(&coord);
        handles.push(tokio::spawn(async move {
            c.run_query(&format!("SELECT {i}"), QueryOverrides::default())
                .await
        }));
    }
    for h in handles {
        let outcome = h.await.unwrap().expect("each concurrent job succeeds");
        assert!(outcome.verified);
    }
}

// --------------------------------------------------------------------------
// HEDGING & TRUST
// --------------------------------------------------------------------------

#[tokio::test]
async fn scenario_hedged_race_fastest_wins_losers_reset() {
    let fast = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let fast2 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let slow = spawn_worker(Arc::new(
        MockEngine::deterministic().with_delay(Duration::from_millis(400)),
    ))
    .await;
    let coord = coordinator(&[&fast, &fast2, &slow], cfg(3, 2), store()).await;

    let outcome = coord
        .run_query("SELECT 1", QueryOverrides::default())
        .await
        .unwrap();
    assert_ne!(outcome.winner.as_ref(), Some(&slow.node_id));
    assert!(outcome.verified);
}

#[tokio::test]
async fn scenario_quorum_accepts_matching_hashes() {
    let a = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let b = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let c = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let coord = coordinator(&[&a, &b, &c], cfg(3, 3), store()).await;

    let outcome = coord
        .run_query("SELECT 1", QueryOverrides::default())
        .await
        .unwrap();
    assert_eq!(outcome.agreement, 3);
    assert!(outcome.verified);
}

#[tokio::test]
async fn scenario_malicious_worker_detected_and_penalized() {
    let h1 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let h2 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let bad = spawn_worker(Arc::new(MockEngine::deterministic().cheating())).await;
    let st = store();
    let coord = coordinator(
        &[&h1, &h2, &bad],
        cfg(3, 2),
        st.clone() as Arc<dyn TrustStore>,
    )
    .await;

    let outcome = coord
        .run_query("SELECT 1", QueryOverrides::default())
        .await
        .unwrap();
    let bad_receipt = outcome
        .receipts
        .iter()
        .find(|r| r.worker_id == bad.node_id)
        .unwrap();
    assert_eq!(bad_receipt.verdict, Verdict::Incorrect);
    assert!(
        p2p_trust::verify_receipt(bad_receipt),
        "receipt must be validly signed"
    );
    assert!(st.penalty(&bad.node_id) > 0.0);
}

#[tokio::test]
async fn scenario_canary_audit_slashes_failing_worker() {
    // A canary query whose correct answer the requester already knows.
    let honest = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let cheater = spawn_worker(Arc::new(MockEngine::deterministic().cheating())).await;

    let st = store();
    let auditor = Arc::new(CanaryAuditor::new(0.0, 64));
    // Known-good answer = the honest deterministic result.
    let sql = "SELECT canary";
    let expected = MockEngine::deterministic()
        .execute(
            sql,
            p2p_node::ExecLease {
                memory_bytes: 1 << 20,
                threads: 1,
            },
        )
        .await
        .unwrap();
    let qh = p2p_proto::QueryHash::compute(sql, "mock-1");
    auditor.register(qh, canonical_hash(&expected));

    let coord = coordinator(
        &[&honest, &cheater],
        cfg(2, 1),
        st.clone() as Arc<dyn TrustStore>,
    )
    .await
    .with_canary(auditor);

    let outcome = coord
        .run_query(sql, QueryOverrides::default())
        .await
        .unwrap();
    // Honest worker matches the canary answer and wins.
    assert_eq!(outcome.winner.as_ref(), Some(&honest.node_id));
    // Cheater is judged Incorrect against the known answer and penalized.
    let bad = outcome
        .receipts
        .iter()
        .find(|r| r.worker_id == cheater.node_id)
        .unwrap();
    assert_eq!(bad.verdict, Verdict::Incorrect);
    assert!(st.penalty(&cheater.node_id) > 0.0);
}

#[tokio::test]
async fn scenario_reputation_evolves_with_recency() {
    // Deterministic, store-level: an old wrong answer is outweighed by newer
    // correct ones (recency decay), so reputation recovers over time.
    let st = InMemoryTrustStore::new(&GridConfig::default().trust, &GridConfig::default().limits);
    let w = NodeId("b3:evolving".into());
    let mk = |verdict, ts| p2p_proto::Receipt {
        job_id: p2p_proto::JobId::new(),
        worker_id: w.clone(),
        requester_id: NodeId("b3:r".into()),
        query_hash: p2p_proto::QueryHash::compute("q", "t"),
        result_hash: "h".into(),
        verdict,
        latency_ms: 1,
        ts,
        observed_input_bytes: 0,
        observed_result_rows: 0,
        observed_result_bytes: 0,
        input_fingerprint: String::new(),
        requester_pubkey: String::new(),
        sig: String::new(),
    };
    let half_life = GridConfig::default().trust.reputation_half_life_secs;
    st.record(&mk(Verdict::Incorrect, 0));
    let early = st.reputation(&w, 0).unwrap();
    assert_eq!(early, 0.0);
    // Many half-lives later, a string of correct results dominates.
    let t = half_life * 5;
    for _ in 0..10 {
        st.record(&mk(Verdict::Correct, t));
    }
    let recovered = st.reputation(&w, t).unwrap();
    assert!(
        recovered > 0.9,
        "reputation should recover, got {recovered}"
    );
}

// --------------------------------------------------------------------------
// ADMISSION CONTROL
// --------------------------------------------------------------------------

#[tokio::test]
async fn scenario_worker_rejects_then_requester_routes_elsewhere() {
    // Worker A serves only Internal data → rejects a Public offer. Worker B
    // serves Public → the requester routes there instead.
    let only_internal = BudgetConfig {
        data_classes: vec![DataClassCfg::Internal],
        ..GridConfig::default().budget
    };
    let a = spawn_worker_with_budget(Arc::new(MockEngine::deterministic()), only_internal).await;
    let b = spawn_worker(Arc::new(MockEngine::deterministic())).await;

    // replicas=1, quorum=1: only B can serve the Public query.
    let coord = coordinator(&[&a, &b], cfg(1, 1), store()).await;
    let outcome = coord
        .run_query("SELECT 1", QueryOverrides::default())
        .await
        .unwrap();
    assert_eq!(outcome.winner.as_ref(), Some(&b.node_id));
}

// --------------------------------------------------------------------------
// VERSIONING
// --------------------------------------------------------------------------

fn vinfo(v: Version, min: Version) -> VersionInfo {
    VersionInfo {
        version: v,
        min_supported: min,
        engine_version: "mock-1".into(),
        extension_version: "0.1.0".into(),
        require_matching_engine: false,
    }
}

#[tokio::test]
async fn scenario_versioning_compatible_and_incompatible() {
    let net = GridConfig::default().network;
    // Compatible: server v1.3 negotiates down to client v1.1.
    let server = QuicTransport::bind_with_version(
        &net,
        &idcfg(),
        NodeIdentity::generate().unwrap(),
        vinfo(Version::new(1, 3, 0), Version::new(1, 0, 0)),
    )
    .unwrap();
    let client = QuicTransport::bind_with_version(
        &net,
        &idcfg(),
        NodeIdentity::generate().unwrap(),
        vinfo(Version::new(1, 1, 0), Version::new(1, 0, 0)),
    )
    .unwrap();
    let sid = server.local_node_id().clone();
    let addr = server.local_addr().unwrap();
    let sc = server.clone();
    let task = tokio::spawn(async move {
        let conn = sc.accept().await.unwrap().unwrap();
        assert_eq!(conn.negotiated().version, Version::new(1, 1, 0));
        tokio::time::sleep(Duration::from_millis(50)).await;
    });
    let conn = client.connect(addr, Some(sid)).await.unwrap();
    assert_eq!(conn.negotiated().version, Version::new(1, 1, 0));
    task.await.unwrap();

    // Incompatible: client below server's min_supported → typed rejection.
    let strict = QuicTransport::bind_with_version(
        &net,
        &idcfg(),
        NodeIdentity::generate().unwrap(),
        vinfo(Version::new(1, 5, 0), Version::new(1, 5, 0)),
    )
    .unwrap();
    let old = QuicTransport::bind_with_version(
        &net,
        &idcfg(),
        NodeIdentity::generate().unwrap(),
        vinfo(Version::new(1, 2, 0), Version::new(1, 0, 0)),
    )
    .unwrap();
    let strict_id = strict.local_node_id().clone();
    let strict_addr = strict.local_addr().unwrap();
    let strict_clone = strict.clone();
    tokio::spawn(async move {
        let _ = strict_clone.accept().await;
    });
    let result = old.connect(strict_addr, Some(strict_id)).await;
    assert!(matches!(
        result,
        Err(p2p_transport::TransportError::IncompatibleVersion(_))
    ));
}

// --------------------------------------------------------------------------
// CONFIGURATION (no-hard-coding)
// --------------------------------------------------------------------------

#[test]
fn scenario_config_precedence_defaults_file_env_percall() {
    // defaults < file < env < per-call SQL params, plus validation rejection.
    let base = GridConfig::default();
    assert_eq!(base.scheduler.replicas, 3);

    // file layer
    let mut cfg = GridConfig::from_toml_str("[scheduler]\nreplicas = 5\nquorum = 3\n").unwrap();
    assert_eq!(cfg.scheduler.replicas, 5);

    // env layer overrides file
    let mut env = std::collections::BTreeMap::new();
    env.insert("P2P_QUORUM".to_string(), "2".to_string());
    cfg.apply_env_map(&env).unwrap();
    assert_eq!(cfg.scheduler.quorum, 2);
    assert_eq!(cfg.scheduler.replicas, 5);

    // per-call overrides env
    let eff = QueryOverrides {
        replicas: Some(7),
        ..Default::default()
    }
    .apply(&cfg)
    .unwrap();
    assert_eq!(eff.scheduler.replicas, 7);

    // invalid config rejected by validation
    let bad = GridConfig::from_toml_str("[scheduler]\nreplicas = 2\nquorum = 9\n").unwrap();
    assert!(bad.validate().is_err());
}

// --------------------------------------------------------------------------
// RESILIENCE / CHURN
// --------------------------------------------------------------------------

#[tokio::test]
async fn scenario_worker_timeout_masked_by_redundancy() {
    let h1 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let h2 = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    // This worker hangs far longer than the dispatch timeout.
    let hung = spawn_worker(Arc::new(
        MockEngine::deterministic().with_delay(Duration::from_secs(30)),
    ))
    .await;

    let mut c = cfg(3, 2);
    c.scheduler.dispatch_timeout_ms = 800; // short, so the hung worker times out
    let coord = coordinator(&[&h1, &h2, &hung], c, store()).await;

    let outcome = coord
        .run_query("SELECT 1", QueryOverrides::default())
        .await
        .unwrap();
    // The two healthy workers still reach quorum despite the hung one.
    assert!(outcome.verified);
    assert_ne!(outcome.winner.as_ref(), Some(&hung.node_id));
}

#[tokio::test]
async fn scenario_churn_discovery_returns_bounded_healthy_set() {
    // Build a membership view from many signed ads; some are stale (left the
    // swarm). Discovery must return a bounded set of only-fresh candidates.
    let table = MembershipTable::new(1000, 8, 30);
    let now = now_ts();
    let mut fresh_ids = Vec::new();
    for p in 0..500u16 {
        let id = NodeIdentity::generate().unwrap();
        let pk = id.public_key_bytes();
        // half are stale (joined/left long ago)
        let ts = if p % 2 == 0 {
            now
        } else {
            now.saturating_sub(10_000)
        };
        let draft = CapabilityDraft {
            addr: format!("127.0.0.1:{}", 20_000 + p),
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
        let ad = sign_capability_ad(draft, &p2p_node::IdentitySigner(&id));
        if ts == now {
            fresh_ids.push(ad.node_id.clone());
        }
        assert!(table.ingest(ad));
    }
    let filter = p2p_node::CandidateFilter {
        data_class: p2p_proto::DataClass::Public,
        min_attestation: AttestationLevel::L0,
        network: None,
        groups: vec![],
        regions: vec![],
    };
    let candidates = table.find_candidates(16, filter).await;
    // bounded
    assert!(candidates.len() <= 16);
    assert!(!candidates.is_empty());
    // healthy only: every returned candidate is a fresh one
    for c in &candidates {
        assert!(
            fresh_ids.contains(c.node_id.as_ref().unwrap()),
            "stale candidate returned"
        );
    }
}

// --------------------------------------------------------------------------
// ECONOMICS: free/paid decoupling (settlement optional, scoring always runs)
// --------------------------------------------------------------------------

/// A FREE job (economics disabled — the default) must still be dispatched,
/// verified, produce a signed receipt, and update the reputation store. Scoring
/// is independent of the settlement layer: no chain, but still scored.
#[tokio::test]
async fn scenario_free_job_is_scored_without_chain() {
    let w = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let st = store();
    // Default config => economics.enabled = false => every job is free.
    let c = cfg(1, 1);
    assert!(
        !c.economics.enabled,
        "default must be the free, no-chain grid"
    );
    let coord = coordinator(&[&w], c, st.clone() as Arc<dyn TrustStore>).await;

    let outcome = coord
        .run_query("SELECT 1", QueryOverrides::default())
        .await
        .unwrap();

    // Grid path (not local), verified, with a signed receipt emitted...
    assert!(!outcome.executed_locally);
    assert!(outcome.verified);
    assert!(
        !outcome.receipts.is_empty(),
        "a free job must still emit receipts"
    );
    let winner = outcome.winner.clone().unwrap();
    // ...and reputation/scoring was updated for the worker from FREE work.
    assert!(
        st.reputation(&winner, now_ts()).is_some(),
        "free work must update the reputation store",
    );
    assert!(st.observation_count(&winner) >= 1);
}

/// A PAID job wires the economics `stake_factor` seam (via a StakeRegistry) and
/// must still complete and score. Proves `with_stake_registry` + the paid gate
/// integrate without breaking the dispatch/scoring path.
#[tokio::test]
async fn scenario_paid_job_uses_stake_seam_and_still_scores() {
    let w = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let st = store();

    let mut c = cfg(1, 1);
    c.economics.enabled = true;
    c.economics.default_payment = PaymentPref::Paid;
    c.economics.settlement = SettlementRail::Channel;
    c.economics.fee_recipient = Some(format!("0:{}", "00".repeat(32)));
    c.validate().unwrap();

    // Stake registry with the worker bonded above the (zero) public minimum.
    let reg = Arc::new(InMemoryStakeRegistry::new(0, 0, 0, 100_000 * 1_000_000_000));
    reg.set_stake(&w.node_id, 1_000 * 1_000_000_000);

    let coord = coordinator(&[&w], c, st.clone() as Arc<dyn TrustStore>)
        .await
        .with_stake_registry(reg);

    let outcome = coord
        .run_query("SELECT 1", QueryOverrides::default())
        .await
        .unwrap();
    assert!(outcome.verified);
    assert!(!outcome.receipts.is_empty());
    // Scoring still runs on the paid path.
    let winner = outcome.winner.clone().unwrap();
    assert!(st.reputation(&winner, now_ts()).is_some());
}

/// The per-call `payment` override forces a job free even when economics is
/// enabled with a paid default — and it remains fully scored.
#[tokio::test]
async fn scenario_per_call_free_override_bypasses_chain_but_scores() {
    let w = spawn_worker(Arc::new(MockEngine::deterministic())).await;
    let st = store();

    let mut c = cfg(1, 1);
    c.economics.enabled = true;
    c.economics.default_payment = PaymentPref::Paid;
    c.economics.settlement = SettlementRail::Channel;
    c.economics.fee_recipient = Some(format!("0:{}", "00".repeat(32)));
    c.validate().unwrap();
    let coord = coordinator(&[&w], c, st.clone() as Arc<dyn TrustStore>).await;

    // payment => 'free' per call: no chain, still scored.
    let overrides = QueryOverrides {
        payment: Some(PaymentPref::Free),
        ..Default::default()
    };
    let outcome = coord.run_query("SELECT 1", overrides).await.unwrap();
    assert!(outcome.verified);
    assert!(!outcome.receipts.is_empty());
    assert!(st
        .reputation(&outcome.winner.clone().unwrap(), now_ts())
        .is_some());
}
