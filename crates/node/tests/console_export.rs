//! Snapshot exporter for the web console (`web/`).
//!
//! This is NOT a pass/fail test — it runs the REAL system (loopback-QUIC grid,
//! real trust engine, real settlement doubles, real canonical hashing, real
//! config) and serializes everything it observes to `web/src/data/snapshot.json`,
//! which the Next.js console reads. Run it explicitly:
//!
//!   cargo test -p p2p-node --test console_export -- --ignored --nocapture
//!
//! Everything here is computed by the actual crates — no hand-authored values.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer, SigningKey};
use serde_json::{json, Value as J};

use p2p_config::{
    GridConfig, IdentityConfig, PaymentPref, PinningMode, QueryOverrides, SettlementRail,
};
use p2p_node::{
    AdmissionController, Candidate, Coordinator, MockEngine, QueryEngine, StaticDiscovery, Worker,
    WorkerParams,
};
use p2p_proto::messages::{
    Bid, BidDecision, DataClass, Dispatch, Offer, ResultCommit, ScopedCredential, VerifyMode,
};
use p2p_proto::{Attestation, AttestationLevel, JobId, NodeId, QueryHash, ResultSet, Value};
use p2p_settlement::base64_encode;
use p2p_settlement::cell::Cell;
use p2p_settlement::merkle;
use p2p_settlement::ton::{
    build_anchor_submit, build_escrow_data, build_escrow_settle, build_escrow_terms,
    build_stake_deposit, build_update_params, candidates_commitment, escrow_code_from_boc_base64,
    EscrowInit, GlobalParams, OP_ANCHOR_SUBMIT, OP_ANCHOR_UPGRADE_CODE, OP_ESCROW_REFUND,
    OP_ESCROW_SETTLE, OP_ESCROW_TOPUP, OP_STAKE_ANNOUNCE_UPGRADE, OP_STAKE_APPLY_UPGRADE,
    OP_STAKE_CANCEL_UPGRADE, OP_STAKE_DEPOSIT, OP_STAKE_SLASH, OP_STAKE_UNBOND, OP_STAKE_WITHDRAW,
    OP_UPDATE_ADMIN, OP_UPDATE_PARAMS, OP_UPGRADE_CODE,
};
use p2p_settlement::ton_proof::{node_bind_message, ton_proof_signing_hash, verify_binding};
use p2p_settlement::traits::Settlement;
use p2p_settlement::types::{Amount, EscrowHandle, SettleError, SettlementOutcome};
use p2p_settlement::types::{NodeWalletBinding, TonProof, WalletAddress};
use p2p_settlement::{
    quality_score, stake_factor, throughput_score, InMemoryRecordAnchor, InMemoryStakeRegistry,
    QualitySample, RecordAnchor, StakeRegistry, WalletV5R1,
};
use p2p_transport::{NodeIdentity, QuicTransport, Transport};
use p2p_trust::{
    age_factor, canonical_hash, evaluate_quorum, exploration_bonus, now_ts, soft_trust_score,
    InMemoryTrustStore, TrustInputs, TrustStore,
};

const TON: Amount = 1_000_000_000;

// --------------------------------------------------------------------------
// Worker bring-up (mirrors crates/node/tests/e2e.rs + scenarios.rs helpers)
// --------------------------------------------------------------------------

struct WorkerHandle {
    alias: String,
    node_id: NodeId,
    addr: SocketAddr,
    attestation: AttestationLevel,
    budget_mem: u64,
    budget_threads: u32,
    max_jobs: u32,
    delay_ms: u64,
    behavior: &'static str, // "honest" | "cheat" | "fail"
    price: u64,             // advertised unit price (whole TON), mirrors console-server
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

fn attest(level: AttestationLevel) -> Attestation {
    match level {
        AttestationLevel::L0 => Attestation::stub_l0(),
        AttestationLevel::L1 => Attestation {
            level: AttestationLevel::L1,
            evidence: b"tpm-quote:pcr0..7".to_vec(),
            measurement: Some("known-good-boot-image".into()),
        },
        AttestationLevel::L2 => Attestation {
            level: AttestationLevel::L2,
            evidence: b"tdx-quote:enclave-report".to_vec(),
            measurement: Some("allowlisted-enclave-v1".into()),
        },
    }
}

struct Spec {
    alias: &'static str,
    level: AttestationLevel,
    mem_gb: u64,
    threads: u32,
    max_jobs: u32,
    delay_ms: u64,
    behavior: &'static str,
    price: u64,
}

async fn spawn_worker(spec: &Spec) -> WorkerHandle {
    let net = GridConfig::default().network;
    let transport =
        Arc::new(QuicTransport::bind(&net, &idcfg(), NodeIdentity::generate().unwrap()).unwrap());

    // Capacity is advertised from the BudgetConfig handed to admission.
    let mut budget = GridConfig::default().budget;
    budget.memory_bytes = spec.mem_gb * 1024 * 1024 * 1024;
    budget.threads = spec.threads;
    budget.max_jobs = spec.max_jobs;
    let admission = AdmissionController::new(&budget);

    let mut cfg = GridConfig::default();
    cfg.budget = budget.clone();
    let params = WorkerParams::from_config(&cfg);

    // Engine behavior → real, observable verdict spread.
    let base = match spec.behavior {
        "cheat" => MockEngine::deterministic().cheating(),
        "fail" => MockEngine::failing("simulated worker fault"),
        _ => MockEngine::deterministic(),
    };
    let engine: Arc<dyn QueryEngine> =
        Arc::new(base.with_delay(Duration::from_millis(spec.delay_ms)));

    let node_id = transport.local_node_id().clone();
    let addr = transport.local_addr().unwrap();
    let worker = Worker::new(
        transport.clone(),
        engine,
        admission,
        attest(spec.level),
        params,
    );
    let task = worker.spawn();

    WorkerHandle {
        alias: spec.alias.to_string(),
        node_id,
        addr,
        attestation: spec.level,
        budget_mem: budget.memory_bytes,
        budget_threads: budget.threads,
        max_jobs: budget.max_jobs,
        delay_ms: spec.delay_ms,
        behavior: spec.behavior,
        price: spec.price,
        _transport: transport,
        _task: task,
    }
}

fn base_cfg(replicas: usize, quorum: usize) -> GridConfig {
    let mut c = GridConfig::default();
    c.scheduler.replicas = replicas;
    c.scheduler.quorum = quorum;
    c.scheduler.offer_timeout_ms = 2_000;
    c.scheduler.dispatch_timeout_ms = 6_000;
    c.scheduler.attempt_deadline_ms = 12_000;
    c.trust.min_trust = 0.0; // fresh, reputation-less workers must be selectable
    c.discovery.candidate_sample_size = 32;
    c
}

async fn make_coordinator(
    workers: &[&WorkerHandle],
    cfg: Arc<GridConfig>,
    store: Arc<dyn TrustStore>,
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
    Coordinator::new(req, disc, store, cfg, "mock-1")
}

// A settlement double that records the FULL real SettlementOutcome the
// coordinator computes (winner/fee/participant split), not just the total.
struct CapturingSettlement {
    inner: p2p_settlement::MockSettlement,
    outcomes: std::sync::Mutex<Vec<SettlementOutcome>>,
}
impl CapturingSettlement {
    fn new() -> Self {
        Self {
            inner: p2p_settlement::MockSettlement::new(),
            outcomes: std::sync::Mutex::new(Vec::new()),
        }
    }
}
impl Settlement for CapturingSettlement {
    fn open_escrow(&self, job: &JobId, max_bid: Amount) -> Result<EscrowHandle, SettleError> {
        self.inner.open_escrow(job, max_bid)
    }
    fn settle(&self, h: &EscrowHandle, outcome: &SettlementOutcome) -> Result<(), SettleError> {
        self.outcomes.lock().unwrap().push(outcome.clone());
        self.inner.settle(h, outcome)
    }
    fn refund(&self, h: &EscrowHandle) -> Result<(), SettleError> {
        self.inner.refund(h)
    }
    fn is_onchain(&self) -> bool {
        true
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn median(mut v: Vec<u64>) -> u64 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    v[v.len() / 2]
}

// --------------------------------------------------------------------------
// The exporter
// --------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "snapshot exporter — run explicitly with --ignored to (re)generate web data"]
async fn export_console_snapshot() {
    let started = Instant::now();
    let generated_at_ms = unix_ms();

    // --- Bring up a real, heterogeneous grid over loopback QUIC --------------
    // This MUST mirror the live console-server demo grid
    // (crates/console-server/src/main.rs) so the static snapshot matches live:
    // same 9 hosts, attestation tiers (4 L2 / 2 L1 / 3 L0), stakes, vouchers and
    // bid prices.
    let specs = [
        Spec {
            alias: "frost-owl",
            level: AttestationLevel::L2,
            mem_gb: 64,
            threads: 32,
            max_jobs: 12,
            delay_ms: 12,
            behavior: "honest",
            price: 7,
        },
        Spec {
            alias: "harbor-vole",
            level: AttestationLevel::L2,
            mem_gb: 96,
            threads: 40,
            max_jobs: 10,
            delay_ms: 22,
            behavior: "honest",
            price: 8,
        },
        Spec {
            alias: "tidal-fox",
            level: AttestationLevel::L2,
            mem_gb: 32,
            threads: 16,
            max_jobs: 6,
            delay_ms: 30,
            behavior: "honest",
            price: 6,
        },
        Spec {
            alias: "marsh-otter",
            level: AttestationLevel::L2,
            mem_gb: 48,
            threads: 24,
            max_jobs: 8,
            delay_ms: 18,
            behavior: "honest",
            price: 7,
        },
        Spec {
            alias: "amber-mole",
            level: AttestationLevel::L1,
            mem_gb: 24,
            threads: 12,
            max_jobs: 4,
            delay_ms: 45,
            behavior: "honest",
            price: 5,
        },
        Spec {
            alias: "pine-marten",
            level: AttestationLevel::L1,
            mem_gb: 16,
            threads: 8,
            max_jobs: 4,
            delay_ms: 60,
            behavior: "honest",
            price: 4,
        },
        Spec {
            alias: "slate-heron",
            level: AttestationLevel::L0,
            mem_gb: 8,
            threads: 4,
            max_jobs: 2,
            delay_ms: 110,
            behavior: "honest",
            price: 3,
        },
        Spec {
            alias: "rust-shrike",
            level: AttestationLevel::L0,
            mem_gb: 8,
            threads: 4,
            max_jobs: 2,
            delay_ms: 18,
            behavior: "cheat",
            price: 5,
        },
        Spec {
            alias: "cobalt-stoat",
            level: AttestationLevel::L0,
            mem_gb: 4,
            threads: 2,
            max_jobs: 1,
            delay_ms: 25,
            behavior: "fail",
            price: 5,
        },
    ];
    let mut workers = Vec::new();
    for s in &specs {
        workers.push(spawn_worker(s).await);
    }
    let wrefs: Vec<&WorkerHandle> = workers.iter().collect();
    let alias_of = |id: &NodeId| -> String {
        workers
            .iter()
            .find(|w| &w.node_id == id)
            .map(|w| w.alias.clone())
            .unwrap_or_else(|| "requester".into())
    };

    // Shared trust store so reputation accumulates across the whole batch.
    let store = Arc::new(InMemoryTrustStore::new(
        &GridConfig::default().trust,
        &GridConfig::default().limits,
    ));

    // --- Run a real batch of free (public) jobs -----------------------------
    // Mirror the console-server warmup: replicas 8 / quorum 6 forces every honest
    // host to commit on each job, so reputation (and thus effective trust) climbs
    // to the same ~0.9 band the live grid shows rather than the old ~0.7.
    let cfg = Arc::new(base_cfg(8, 6));
    let coord = make_coordinator(&wrefs, cfg.clone(), store.clone()).await;

    let queries = [
        "SELECT region, count(*) AS orders, sum(total) AS gmv FROM orders GROUP BY region ORDER BY gmv DESC",
        "SELECT date_trunc('hour', ts) h, avg(latency_ms) FROM telemetry GROUP BY 1 ORDER BY 1",
        "SELECT sku, sum(qty) FROM sales GROUP BY sku HAVING sum(qty) > 1000",
        "SELECT cohort, count(DISTINCT user_id) active FROM events WHERE kind='session' GROUP BY cohort",
        "SELECT p99(duration_ms), count(*) FROM spans WHERE service='api'",
        "SELECT country, sum(revenue) FROM invoices GROUP BY country ORDER BY 2 DESC LIMIT 20",
        "SELECT bucket, approx_count_distinct(id) FROM impressions GROUP BY bucket",
        "SELECT model, avg(score) FROM predictions GROUP BY model",
        "SELECT day, sum(bytes)/1e9 gb FROM egress GROUP BY day",
        "SELECT status, count(*) FROM jobs GROUP BY status",
        "SELECT region, percentile_cont(0.5) WITHIN GROUP (ORDER BY rtt) FROM pings GROUP BY region",
        "SELECT tier, sum(amount) FROM payments GROUP BY tier",
    ];

    #[derive(Clone)]
    struct JobRec {
        id: String,
        sql: String,
        verified: bool,
        quorum: usize,
        agreement: usize,
        winner: Option<NodeId>,
        agreed_hash: Option<String>,
        row_count: usize,
        latency_ms: u64,
        created_ms: u64,
        result: ResultSet,
        receipts: Vec<p2p_proto::Receipt>,
    }
    let mut job_recs: Vec<JobRec> = Vec::new();
    // Several passes so reputation history is deep enough for the confident
    // (Wilson-shrunk) reputation — and hence effective trust — to saturate near
    // the live grid's ~0.9 band (the live server accrues this over its warmup +
    // ambient jobs).
    for pass in 0..12u32 {
        for (i, q) in queries.iter().enumerate() {
            let t0 = Instant::now();
            let created = unix_ms();
            let sql = if pass == 0 {
                q.to_string()
            } else {
                format!("{q} -- pass{pass} {i}")
            };
            if let Ok(o) = coord.run_query(&sql, QueryOverrides::default()).await {
                let lat = t0.elapsed().as_millis() as u64;
                job_recs.push(JobRec {
                    id: o.job_id.0.clone(),
                    sql: sql.clone(),
                    verified: o.verified,
                    quorum: o.quorum,
                    agreement: o.agreement,
                    winner: o.winner.clone(),
                    agreed_hash: o.agreed_hash.clone(),
                    row_count: o.result.row_count(),
                    latency_ms: lat,
                    created_ms: created,
                    result: o.result.clone(),
                    receipts: o.receipts.clone(),
                });
            }
        }
    }

    // Real vouches — same set/strength as the console-server demo so voucher_trust
    // matches live (top L2 hosts 0.8, L1 hosts 0.7).
    let vouchers: BTreeMap<&str, f64> = [
        ("frost-owl", 0.8),
        ("harbor-vole", 0.8),
        ("tidal-fox", 0.8),
        ("marsh-otter", 0.8),
        ("amber-mole", 0.7),
        ("pine-marten", 0.7),
    ]
    .into_iter()
    .collect();
    for w in &workers {
        if let Some(v) = vouchers.get(w.alias.as_str()) {
            store.add_vouch(&w.node_id, *v);
        }
    }

    // --- Stake registry (real stake_factor) ---------------------------------
    // Stakes mirror the console-server demo exactly.
    let eco = GridConfig::default().economics;
    let reg = Arc::new(InMemoryStakeRegistry::from_config(&eco.stake));
    let stakes_ton: BTreeMap<&str, u64> = [
        ("frost-owl", 4200),
        ("harbor-vole", 3000),
        ("tidal-fox", 2600),
        ("marsh-otter", 3400),
        ("amber-mole", 1500),
        ("pine-marten", 1200),
        ("slate-heron", 120),
    ]
    .into_iter()
    .collect();
    for w in &workers {
        if let Some(t) = stakes_ton.get(w.alias.as_str()) {
            reg.set_stake(&w.node_id, *t as Amount * TON);
        }
    }

    // --- Per-worker REAL trust / reputation / capacity ----------------------
    let now = now_ts();
    let weights = &cfg.trust.weights;
    let mut workers_json: Vec<J> = Vec::new();
    // Aggregate verdict tallies per worker from all receipts.
    let mut correct: BTreeMap<String, u64> = BTreeMap::new();
    let mut faults: BTreeMap<String, u64> = BTreeMap::new();
    let mut lats: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    let mut parts: BTreeMap<String, u64> = BTreeMap::new();
    for jr in &job_recs {
        for r in &jr.receipts {
            let id = r.worker_id.0.clone();
            *parts.entry(id.clone()).or_default() += 1;
            if r.verdict.is_correct() {
                *correct.entry(id.clone()).or_default() += 1;
                lats.entry(id.clone()).or_default().push(r.latency_ms);
            } else if r.verdict.is_provider_fault() {
                *faults.entry(id.clone()).or_default() += 1;
            }
        }
    }

    for w in &workers {
        let id = w.node_id.0.clone();
        let obs = store.observation_count(&w.node_id);
        let rep_raw = store.reputation(&w.node_id, now);
        let rep_conf = store
            .confident_reputation(
                &w.node_id,
                now,
                eco.reputation.prior_alpha,
                eco.reputation.prior_beta,
                eco.reputation.confidence_z,
            )
            .unwrap_or(cfg.trust.bootstrap_trust);
        let vouch = store.voucher_trust(&w.node_id).min(1.0);
        let penalty = store.penalty(&w.node_id);
        let sf = reg.stake_factor(&w.node_id);
        let inputs = TrustInputs {
            reputation: rep_conf,
            age_factor: age_factor(obs, 20),
            voucher_trust: vouch,
            stake_factor: sf,
            penalties: penalty,
        };
        let soft = soft_trust_score(weights, &inputs);
        let expl = exploration_bonus(
            obs,
            eco.ranking.exploration_rate,
            eco.ranking.exploration_saturation,
        );
        let effective = (soft + expl).clamp(0.0, 1.0);
        let c = *correct.get(&id).unwrap_or(&0);
        let f = *faults.get(&id).unwrap_or(&0);
        let success = if c + f == 0 {
            1.0
        } else {
            c as f64 / (c + f) as f64
        };
        let p50 = median(lats.get(&id).cloned().unwrap_or_default());
        let stake_ton = stakes_ton.get(w.alias.as_str()).copied().unwrap_or(0);

        workers_json.push(json!({
            "id": id,
            "alias": w.alias,
            "attestation": w.attestation.as_str(),
            "behavior": w.behavior,
            "trust": effective,
            "soft": soft,
            "reputation": rep_raw,
            "reputationConfident": rep_conf,
            "observations": obs,
            "ageFactor": age_factor(obs, 20),
            "voucherTrust": vouch,
            "stakeFactor": sf,
            "penalty": penalty,
            "explorationBonus": expl,
            "stakeTon": stake_ton,
            "stakeNanoton": (stake_ton as Amount * TON).to_string(),
            "priceTon": w.price,
            "totalMemBytes": w.budget_mem,
            "totalThreads": w.budget_threads,
            "maxJobs": w.max_jobs,
            "jobsParticipated": *parts.get(&id).unwrap_or(&0),
            "correct": c,
            "faults": f,
            "successRate": success,
            "p50LatencyMs": p50,
            "delayMs": w.delay_ms,
            "online": w.behavior != "fail",
            "engineVersion": "mock-1",
            "wallet": if stake_ton > 0 { Some(format!("0:{}", hex::encode(blake3::hash(id.as_bytes()).as_bytes()))) } else { None },
        }));
    }

    // --- Build detailed job records (first 6) -------------------------------
    let mut jobs_json: Vec<J> = Vec::new();
    let detailed: Vec<&JobRec> = job_recs.iter().take(6).collect();
    for (idx, jr) in detailed.iter().enumerate() {
        let winner = jr.winner.clone();
        let mut cands: Vec<J> = Vec::new();
        // sort receipts by latency for a realistic race order
        let mut rs: Vec<&p2p_proto::Receipt> = jr.receipts.iter().collect();
        rs.sort_by_key(|r| {
            if r.latency_ms == 0 {
                u64::MAX
            } else {
                r.latency_ms
            }
        });
        for r in &rs {
            let is_winner = winner.as_ref() == Some(&r.worker_id);
            let agreeing =
                jr.agreed_hash.as_deref() == Some(r.result_hash.as_str()) && r.verdict.is_correct();
            let state = if is_winner {
                "won"
            } else if r.verdict.is_correct() && agreeing {
                "reset" // agreeing loser: committed a matching hash, then RESET
            } else if r.verdict == p2p_proto::Verdict::Incorrect {
                "committed" // committed a divergent hash
            } else {
                "dispatched" // timed out / no usable commit
            };
            let wkr = workers.iter().find(|w| w.node_id == r.worker_id);
            cands.push(json!({
                "workerId": r.worker_id.0,
                "alias": alias_of(&r.worker_id),
                "attestation": wkr.map(|w| w.attestation.as_str()).unwrap_or("L0"),
                "state": state,
                "verdict": format!("{:?}", r.verdict),
                "etaMs": r.latency_ms,
                "price": wkr.map(|w| w.price).unwrap_or(0),
                "progressPct": if r.verdict.is_correct() || r.verdict == p2p_proto::Verdict::Incorrect { 100 } else { 0 },
                "committedHash": if r.result_hash.is_empty() { J::Null } else { json!(r.result_hash) },
                "commitLatencyMs": r.latency_ms,
            }));
        }
        // Real timeline derived from real per-candidate commit latencies.
        let mut agree_lats: Vec<u64> = jr
            .receipts
            .iter()
            .filter(|r| {
                jr.agreed_hash.as_deref() == Some(r.result_hash.as_str()) && r.verdict.is_correct()
            })
            .map(|r| r.latency_ms)
            .collect();
        agree_lats.sort_unstable();
        let verify_ms = agree_lats
            .get(jr.quorum.saturating_sub(1))
            .copied()
            .unwrap_or(jr.latency_ms);
        let first_commit = agree_lats.first().copied().unwrap_or(0);
        let mut timeline = vec![
            json!({"tMs": 0, "stage": "offer", "label": format!("Offer broadcast to {} candidates", jr.receipts.len()), "detail": "query_hash bound with fresh nonce"}),
            json!({"tMs": 2, "stage": "bidding", "label": "Bids collected (accept/reject)", "detail": "k selected by trust + ETA"}),
            json!({"tMs": 4, "stage": "dispatch", "label": "Dispatch full SQL to top-k", "detail": "scoped per node key"}),
            json!({"tMs": first_commit, "stage": "commit", "label": "First result_hash committed", "detail": "commit-first, before bulk stream"}),
        ];
        if jr.verified {
            timeline.push(json!({"tMs": verify_ms, "stage": "verify", "label": format!("Quorum reached: {}/{} agree", jr.agreement, jr.quorum), "detail": jr.agreed_hash.clone()}));
            timeline.push(json!({"tMs": jr.latency_ms, "stage": "settle", "label": "Winner streams result · losers RESET", "detail": "receipts emitted"}));
        } else {
            timeline.push(json!({"tMs": verify_ms, "stage": "verify", "label": format!("Quorum NOT reached ({}/{} agree)", jr.agreement, jr.quorum), "detail": "re-dispatch recommended"}));
        }

        // Real result preview (winner's actual ResultSet).
        let cols: Vec<String> = jr.result.columns.clone();
        let rows: Vec<Vec<String>> = jr
            .result
            .rows
            .iter()
            .take(8)
            .map(|row| row.iter().map(value_to_string).collect())
            .collect();

        jobs_json.push(json!({
            "id": jr.id,
            "sql": jr.sql,
            "fn": "p2p_query",
            "dataClass": "Public",
            "verifyMode": "Quorum",
            "quorum": jr.quorum,
            "k": jr.receipts.len(),
            "status": if jr.verified { "verified" } else { "failed" },
            "paid": false,
            "requester": "analyst-ws",
            "createdAtMs": jr.created_ms,
            "rowCount": jr.row_count,
            "resultHash": jr.agreed_hash,
            "latencyMs": jr.latency_ms,
            "escrowTon": 0,
            "winner": winner.as_ref().map(&alias_of),
            "winnerId": winner.as_ref().map(|w| w.0.clone()),
            "source": "in-process mock engine (loopback)",
            "candidates": cands,
            "timeline": timeline,
            "result": { "columns": cols, "rows": rows },
            "_idx": idx,
        }));
    }

    // --- Real receipts table (flatten, most recent first) -------------------
    let mut receipts_json: Vec<J> = Vec::new();
    for jr in job_recs.iter().take(8) {
        for r in &jr.receipts {
            let fault = if r.verdict.is_correct() {
                "neutral"
            } else if r.verdict.is_provider_fault() {
                "provider"
            } else {
                "neutral"
            };
            receipts_json.push(json!({
                "jobId": r.job_id.0,
                "workerId": r.worker_id.0,
                "workerAlias": alias_of(&r.worker_id),
                "requesterId": r.requester_id.0,
                "verdict": format!("{:?}", r.verdict),
                "fault": fault,
                "latencyMs": r.latency_ms,
                "tsMs": r.ts * 1000,
                "resultHash": if r.result_hash.is_empty() { "—".to_string() } else { r.result_hash.clone() },
                "sig": format!("ed25519:{}…{}", &r.sig.get(0..8).unwrap_or(""), &r.sig.get(r.sig.len().saturating_sub(4)..).unwrap_or("")),
                "verified": p2p_trust::verify_receipt(r),
                "gossiped": true,
            }));
        }
    }

    // --- REAL canonical hash + quorum examples ------------------------------
    let sample_rs = ResultSet::new(
        vec!["region".into(), "orders".into(), "gmv".into()],
        vec![
            vec![
                Value::Text("emea".into()),
                Value::Int(184_233),
                Value::Float(2_481_002.50),
            ],
            vec![
                Value::Text("amer".into()),
                Value::Int(201_980),
                Value::Float(3_120_550.00),
            ],
            vec![
                Value::Text("apac".into()),
                Value::Int(98_120),
                Value::Float(1_044_980.75),
            ],
        ],
    );
    let sample_hash = canonical_hash(&sample_rs);
    // Reorder rows → SAME hash (order-independent canonicalization).
    let reordered = ResultSet::new(
        sample_rs.columns.clone(),
        sample_rs.rows.iter().rev().cloned().collect(),
    );
    let reordered_hash = canonical_hash(&reordered);
    let q = evaluate_quorum(
        [
            sample_hash.as_str(),
            sample_hash.as_str(),
            reordered_hash.as_str(),
            "b3:divergent",
        ]
        .into_iter(),
        3,
    );

    // --- REAL settlement: a paid run capturing the actual split -------------
    let mut paid_cfg = base_cfg(4, 3);
    paid_cfg.economics.enabled = true;
    paid_cfg.economics.default_payment = PaymentPref::Paid;
    paid_cfg.economics.settlement = SettlementRail::Channel;
    paid_cfg.economics.fee_recipient = Some(format!("0:{}", "00".repeat(32)));
    paid_cfg.economics.pricing.max_bid = 100; // whole TON escrow B
    let paid_cfg = Arc::new(paid_cfg);

    let pay_workers: Vec<&WorkerHandle> = workers
        .iter()
        .filter(|w| w.behavior == "honest")
        .take(4)
        .collect();
    let settlement = Arc::new(CapturingSettlement::new());
    let anchor = Arc::new(InMemoryRecordAnchor::new());
    let pcoord = make_coordinator(&pay_workers, paid_cfg.clone(), store.clone())
        .await
        .with_stake_registry(reg.clone())
        .with_settlement(settlement.clone())
        .with_record_anchor(anchor.clone());

    let mut paid_jobs: Vec<J> = Vec::new();
    for (i, q) in [
        "SELECT cohort, count(DISTINCT user_id) FROM events GROUP BY cohort",
        "SELECT sku, sum(qty*price) gmv FROM line_items GROUP BY sku ORDER BY gmv DESC LIMIT 50",
        "SELECT segment, avg(ltv) FROM customers GROUP BY segment",
    ]
    .iter()
    .enumerate()
    {
        let t0 = Instant::now();
        let created = unix_ms();
        if let Ok(o) = pcoord.run_query(q, QueryOverrides::default()).await {
            paid_jobs.push(json!({
                "id": o.job_id.0,
                "sql": q,
                "fn": "p2p_query",
                "dataClass": "Internal",
                "verifyMode": "Quorum",
                "quorum": o.quorum,
                "k": o.receipts.len(),
                "status": if o.verified { "settled" } else { "failed" },
                "paid": true,
                "requester": "etl-runner",
                "createdAtMs": created,
                "rowCount": o.result.row_count(),
                "resultHash": o.agreed_hash,
                "latencyMs": t0.elapsed().as_millis() as u64,
                "escrowTon": 100,
                "winner": o.winner.as_ref().map(&alias_of),
                "winnerId": o.winner.as_ref().map(|w| w.0.clone()),
                "source": "in-process mock engine (loopback)",
                "candidates": [],
                "timeline": [
                    json!({"tMs":0,"stage":"offer","label":"Offer to paid pool","detail":"escrow B=100 TON"}),
                    json!({"tMs":4,"stage":"dispatch","label":"Dispatch + open escrow","detail":"HTLC keyed on quorum hash"}),
                    json!({"tMs": t0.elapsed().as_millis() as u64,"stage":"settle","label":"Settle: winner paid, losers RESET","detail":"anchored to epoch root"}),
                ],
                "result": {"columns": o.result.columns, "rows": o.result.rows.iter().take(6).map(|r| r.iter().map(value_to_string).collect::<Vec<_>>()).collect::<Vec<_>>()},
                "_paidIdx": i,
            }));
        }
    }

    let events = settlement.inner.events();
    let splits = settlement.outcomes.lock().unwrap().clone();
    let split_json: Vec<J> = splits
        .iter()
        .map(|s| {
            json!({
                "winnerTon": s.winner.amount as f64 / TON as f64,
                "platformFeeTon": s.platform_fee as f64 / TON as f64,
                "participants": s.participants.iter().map(|p| json!({
                    "wallet": format!("0:{}", hex::encode(&p.to.hash[..6])),
                    "amountTon": p.amount as f64 / TON as f64,
                })).collect::<Vec<_>>(),
                "totalTon": s.total() as f64 / TON as f64,
                "resultHashHex": hex::encode(s.result_hash),
            })
        })
        .collect();
    let events_json: Vec<J> = events
        .iter()
        .map(|e| match e {
            p2p_settlement::SettlementEvent::Opened { job, max_bid } => {
                json!({"type":"Opened","job":job.0,"maxBidTon": *max_bid as f64 / TON as f64})
            }
            p2p_settlement::SettlementEvent::Settled { job, total } => {
                json!({"type":"Settled","job":job.0,"totalTon": *total as f64 / TON as f64})
            }
            p2p_settlement::SettlementEvent::Refunded { job } => {
                json!({"type":"Refunded","job":job.0})
            }
        })
        .collect();

    // Real epoch Merkle root + inclusion proof for the first paid job.
    let epoch_root = anchor.epoch_root();
    let mut inclusion = J::Null;
    if let Some(first) = paid_jobs.first() {
        let jid = JobId(first["id"].as_str().unwrap().to_string());
        if let Some(proof) = anchor.prove_inclusion(&jid) {
            let ok = merkle::verify_inclusion(&epoch_root, &proof);
            inclusion = json!({
                "leafHex": hex::encode(proof.leaf),
                "siblings": proof.siblings.iter().map(|(right, h)| json!({"right": right, "hashHex": hex::encode(h)})).collect::<Vec<_>>(),
                "verified": ok,
            });
        }
    }

    // Real stake table.
    let stake_table: Vec<J> = workers
        .iter()
        .filter(|w| stakes_ton.contains_key(w.alias.as_str()))
        .map(|w| {
            json!({
                "alias": w.alias,
                "workerId": w.node_id.0,
                "wallet": format!("0:{}", hex::encode(blake3::hash(w.node_id.0.as_bytes()).as_bytes())),
                "stakeTon": stakes_ton[w.alias.as_str()],
                "stakeFactor": reg.stake_factor(&w.node_id),
                "eligiblePublic": reg.is_eligible(&w.node_id, p2p_config::DataClassCfg::Public),
                "eligibleInternal": reg.is_eligible(&w.node_id, p2p_config::DataClassCfg::Internal),
                "eligibleSensitive": reg.is_eligible(&w.node_id, p2p_config::DataClassCfg::Sensitive),
            })
        })
        .collect();

    // Real quality scores from a real winner sample.
    let qsample = QualitySample::new(0.98, 240, 12 * 1024 * 1024 * 1024);
    let quality = json!({
        "sample": {"successRatio": 0.98, "latencyMs": 240, "bytesVerified": 12u64 * 1024 * 1024 * 1024},
        "throughputScore": throughput_score(240, 12 * 1024 * 1024 * 1024, &eco.quality),
        "qualityScore": quality_score(&qsample, &eco.quality),
    });

    // Real stake_factor curve.
    let stake_curve: Vec<J> = [0u64, 100, 300, 800, 1500, 2600, 4200, 8000, 50000, 100000]
        .iter()
        .map(|t| {
            json!({"stakeTon": t, "factor": stake_factor(*t as Amount * TON, eco.stake.min_stake.max(1) as Amount * TON, eco.stake.stake_cap as Amount * TON)})
        })
        .collect();

    // --- REAL two-way wallet<->node binding (ton_proof) ---------------------
    let node_sk = SigningKey::generate(&mut rand::rngs::OsRng);
    let wallet_sk = SigningKey::generate(&mut rand::rngs::OsRng);
    let node_pk = node_sk.verifying_key().to_bytes();
    let wallet_pk = wallet_sk.verifying_key().to_bytes();
    let node_id_b = format!("b3:{}", hex::encode(blake3::hash(&node_pk).as_bytes()));
    // The wallet address MUST be the v5r1 address derived from the wallet key:
    // `verify_binding` re-derives it and rejects a mismatched (key, address) pair.
    let waddr = WalletV5R1::testnet(wallet_pk).address();
    let nonce = b"console-nonce-01".to_vec();
    let expiry = now + 30 * 24 * 3600;
    let msg1 = node_bind_message(&waddr, &nonce, expiry);
    let sig_node = node_sk.sign(&msg1).to_bytes().to_vec();
    let mut payload = node_id_b.as_bytes().to_vec();
    payload.extend_from_slice(&nonce);
    // domain must equal EXPECTED_TON_PROOF_DOMAIN ("duckdb-p2p") and the
    // timestamp must be fresh within 15 min of the `now` we verify at.
    let mut proof = TonProof {
        domain: "duckdb-p2p".into(),
        timestamp: now,
        payload: payload.clone(),
        signature: vec![],
    };
    let digest = ton_proof_signing_hash(&waddr, &proof);
    proof.signature = wallet_sk.sign(&digest).to_bytes().to_vec();
    let binding = NodeWalletBinding {
        node_id: node_id_b.clone(),
        wallet_address: waddr,
        node_pubkey: node_pk,
        wallet_pubkey: wallet_pk,
        nonce: nonce.clone(),
        expiry,
        sig_node: sig_node.clone(),
        ton_proof: proof.clone(),
    };
    let binding_ok = verify_binding(&binding, now).is_ok();
    let binding_json = json!({
        "nodeId": node_id_b,
        "walletAddress": waddr.to_raw_string(),
        "nonceHex": hex::encode(&nonce),
        "expiry": expiry,
        "sigNodeHex": format!("{}…", &hex::encode(&sig_node)[..16]),
        "tonProof": {
            "domain": proof.domain,
            "timestamp": proof.timestamp,
            "payloadHex": format!("{}…", &hex::encode(&payload)[..16]),
            "signatureHex": format!("{}…", &hex::encode(&proof.signature)[..16]),
        },
        "verified": binding_ok,
    });

    // --- REAL protocol message samples (serialized actual wire types) -------
    let sample_job = JobId::new();
    let req_id = NodeId::from_pubkey(b"requester-demo-key");
    let wid = workers[0].node_id.clone();
    let qh = QueryHash::compute(queries[0], "mock-1");
    let offer = Offer {
        job_id: sample_job.clone(),
        requester_id: req_id.clone(),
        query_hash: qh.clone(),
        cost_hint_rows: Some(184_000_000),
        data_class: DataClass::Internal,
        nonce: 0x9f2a_1c9e_b41d_77a0,
        network: None,
        groups: Vec::new(),
        regions: Vec::new(),
        group_proof: None,
    };
    let bid = Bid {
        job_id: sample_job.clone(),
        worker_id: wid.clone(),
        decision: BidDecision::Accept,
        eta_ms: 2600,
        price: workers[0].price,
        attestation: attest(AttestationLevel::L2),
        recent_receipts: vec![],
        free_mem_bytes: 22 * 1024 * 1024 * 1024,
        free_threads: 14,
        region_proof: None,
    };
    let dispatch = Dispatch {
        job_id: sample_job.clone(),
        sql: queries[0].to_string(),
        query_hash: qh.clone(),
        credential: Some(ScopedCredential {
            provider: "s3".into(),
            token: "<downscoped-sts-session>".into(),
            prefix: "s3://acme-lake/orders/2026/".into(),
            expires_at: now + 900,
        }),
        memory_limit_bytes: 4 * 1024 * 1024 * 1024,
        threads: 8,
        verify_mode: VerifyMode::Quorum,
        sealed_key: None,
        result_parallelism: Some(4),
        compression: None,
    };
    let commit = ResultCommit {
        job_id: sample_job.clone(),
        worker_id: wid.clone(),
        result_hash: sample_hash.clone(),
        row_count: 3,
        latency_ms: 2410,
    };
    let sample_receipt = job_recs
        .iter()
        .flat_map(|j| j.receipts.iter())
        .find(|r| r.verdict.is_correct());
    let protocol_messages = json!({
        "Offer": serde_json::to_value(&offer).unwrap(),
        "Bid": serde_json::to_value(&bid).unwrap(),
        "Dispatch": serde_json::to_value(&dispatch).unwrap(),
        "ResultCommit": serde_json::to_value(&commit).unwrap(),
        "Receipt": sample_receipt.map(|r| serde_json::to_value(r).unwrap()).unwrap_or(J::Null),
    });

    // --- Overview aggregates from the REAL batch ----------------------------
    let verified_count = job_recs.iter().filter(|j| j.verified).count();
    let failed_count = job_recs.len() - verified_count;
    // Cap to the most recent 60 points, mirroring the live server's SERIES_CAP.
    let series_skip = job_recs.len().saturating_sub(60);
    let series: Vec<J> = job_recs
        .iter()
        .enumerate()
        .skip(series_skip)
        .map(|(i, j)| json!({"label": format!("j{}", i + 1), "latencyMs": j.latency_ms, "verified": if j.verified {1} else {0}}))
        .collect();
    // latency histogram from real per-candidate latencies
    let mut buckets = [0u64; 6];
    let blabels = ["<25ms", "25–50", "50–100", "100–250", "250ms–1s", ">1s"];
    for jr in &job_recs {
        for r in &jr.receipts {
            if !r.verdict.is_correct() {
                continue;
            }
            let l = r.latency_ms;
            let b = if l < 25 {
                0
            } else if l < 50 {
                1
            } else if l < 100 {
                2
            } else if l < 250 {
                3
            } else if l < 1000 {
                4
            } else {
                5
            };
            buckets[b] += 1;
        }
    }
    let latency_hist: Vec<J> = blabels
        .iter()
        .enumerate()
        .map(|(i, b)| json!({"bucket": b, "count": buckets[i]}))
        .collect();
    let mut att_counts: BTreeMap<&str, u64> = BTreeMap::new();
    for w in &workers {
        *att_counts.entry(w.attestation.as_str()).or_default() += 1;
    }
    let att_fill = |l: &str| match l {
        "L2" => "var(--chart-1)",
        "L1" => "var(--chart-2)",
        _ => "var(--chart-3)",
    };
    let attestation_mix: Vec<J> = ["L0", "L1", "L2"]
        .iter()
        .map(|l| json!({"level": format!("{} — {}", l, match *l {"L2"=>"TEE enclave","L1"=>"measured boot",_=>"anonymous"}), "count": att_counts.get(l).copied().unwrap_or(0), "fill": att_fill(l)}))
        .collect();

    let avg_trust = {
        let vals: Vec<f64> = workers_json
            .iter()
            .map(|w| w["trust"].as_f64().unwrap_or(0.0))
            .collect();
        vals.iter().sum::<f64>() / vals.len() as f64
    };
    let total_stake: u64 = stakes_ton.values().sum();
    let free_mem: u64 = workers.iter().map(|w| w.budget_mem).sum();

    // --- Real config dump + example TOML ------------------------------------
    let config_value = serde_json::to_value(GridConfig::default()).unwrap();
    let example_toml = std::fs::read_to_string(format!(
        "{}/../../config/p2p.example.toml",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap_or_default();

    // --- REAL on-chain TON artifacts (computed offline by p2p_settlement) ---
    let gp = GlobalParams::from_economics(&eco);
    gp.validate().expect("on-chain GlobalParams validate");
    let treasury = WalletAddress::new(0, *blake3::hash(b"treasury-demo").as_bytes());
    let gp_body = build_update_params(0, &treasury, &gp);
    let eco_cell: Cell = gp_body.cell.refs()[0].clone();
    let eco_cell_hash = hex::encode(eco_cell.repr_hash());
    let eco_cell_boc = base64_encode(&eco_cell.to_boc());

    let body_json = |label: &str, opcode: u32, cell: &Cell| {
        json!({
            "label": label,
            "opcodeHex": format!("0x{opcode:08x}"),
            "opcode": opcode,
            "cellHash": hex::encode(cell.repr_hash()),
            "bocBase64": base64_encode(&cell.to_boc()),
            "bits": cell.bit_len(),
        })
    };

    // Per-job JobEscrow address — derived OFFLINE from the compiled contract code
    // + the real escrow terms (HTLC lock = the real quorum result hash).
    let escrow_artifact = std::fs::read_to_string(format!(
        "{}/../../ton/build/JobEscrow.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .ok();
    let split0 = splits.first().cloned();
    let real_result_hash: [u8; 32] = split0.as_ref().map(|s| s.result_hash).unwrap_or([0u8; 32]);
    let escrow_b: Amount = paid_cfg.economics.pricing.max_bid as Amount * TON;
    let mut ton_escrow = J::Null;
    let mut ton_messages: Vec<J> = Vec::new();
    if let Some(art) = &escrow_artifact {
        if let Ok(v) = serde_json::from_str::<J>(art) {
            if let Some(code_b64) = v.get("code_boc64").and_then(|x| x.as_str()) {
                if let Ok(code) = escrow_code_from_boc_base64(code_b64) {
                    // B1: the requester's pre-committed payout-eligible candidate set
                    // = the winner ∪ each participant of the verdict split. The
                    // escrow terms commit to its hash; settle re-presents the SAME
                    // set, so the on-chain candidatesCommitment check passes.
                    let candidates: Vec<WalletAddress> = split0
                        .as_ref()
                        .map(|s| {
                            let mut c = vec![s.winner.to];
                            for p in &s.participants {
                                if !c.contains(&p.to) {
                                    c.push(p.to);
                                }
                            }
                            c
                        })
                        .unwrap_or_default();
                    let candidates_hash = candidates_commitment(&candidates);
                    let terms =
                        build_escrow_terms(&treasury, &real_result_hash, &candidates_hash, 1);
                    let terms_hash = hex::encode(terms.repr_hash());
                    let init = EscrowInit {
                        requester: WalletAddress::new(
                            0,
                            *blake3::hash(b"requester-demo").as_bytes(),
                        ),
                        arbiter: WalletAddress::new(0, *blake3::hash(b"arbiter-demo").as_bytes()),
                        escrow_amount: escrow_b,
                        deadline: (now + 3600) as u32,
                    };
                    let data = build_escrow_data(&init, terms);
                    let addr = WalletAddress::from_state_init(0, &code, &data);
                    let code_hash = hex::encode(code.repr_hash());
                    ton_escrow = json!({
                        "address": addr.to_raw_string(),
                        "codeHash": code_hash,
                        "termsCellHash": terms_hash,
                        "expectedHashHex": hex::encode(real_result_hash),
                        "candidatesHashHex": hex::encode(candidates_hash),
                        "paramsVersion": 1,
                        "escrowTon": paid_cfg.economics.pricing.max_bid,
                        "deterministic": "address = hash(StateInit{code, data}); data binds requester+arbiter+B+deadline+^terms (terms bind expectedHash+candidatesHash+paramsVersion)",
                    });
                    // A real EscrowSettle message keyed on the real quorum hash,
                    // presenting the committed candidate set.
                    if let Some(s) = &split0 {
                        let settle = build_escrow_settle(
                            1,
                            &s.result_hash,
                            &s.winner.to,
                            s.winner.amount,
                            s.platform_fee,
                            &s.participants,
                            &candidates,
                        );
                        ton_messages.push(body_json(
                            "EscrowSettle (HTLC release)",
                            settle.opcode,
                            &settle.cell,
                        ));
                    }
                }
            }
        }
    }
    // More real message bodies (op-coded TL-B cells the node actually broadcasts).
    let deposit = build_stake_deposit(1, 50 * TON);
    ton_messages.push(body_json(
        "StakeDeposit (bond 50 TON)",
        deposit.opcode,
        &deposit.cell,
    ));
    let anchor_msg = build_anchor_submit(1, 1, &epoch_root, &[0u8; 32], 0);
    ton_messages.push(body_json(
        "AnchorSubmit (epoch 1 root)",
        anchor_msg.opcode,
        &anchor_msg.cell,
    ));
    let refund = build_update_params(2, &treasury, &gp);
    ton_messages.push(body_json(
        "UpdateParams (set GlobalParams)",
        refund.opcode,
        &refund.cell,
    ));

    let opgrp = |pairs: &[(&str, u32)]| -> Vec<J> {
        pairs
            .iter()
            .map(|(n, op)| json!({"name": n, "hex": format!("0x{op:08x}"), "value": op}))
            .collect::<Vec<_>>()
    };
    let ton_computed = json!({
        "globalParams": {
            "platformFeeBps": gp.platform_fee_bps,
            "surchargeBps": gp.surcharge_bps,
            "participationCommissionBps": gp.participation_commission_bps,
            "slashWrongBps": gp.slash_wrong_bps,
            "slashCheatBps": gp.slash_cheat_bps,
            "slashDowntimeBps": gp.slash_downtime_bps,
            "slashEquivocationBps": gp.slash_equivocation_bps,
            "slashFailedCommitmentBps": gp.slash_failed_commitment_bps,
            "splitChallengerBps": gp.split_challenger_bps,
            "splitRedundancyBps": gp.split_redundancy_bps,
            "splitBurnBps": gp.split_burn_bps,
            "splitTreasuryBps": gp.split_treasury_bps,
            "minStakeTon": (gp.min_stake / TON) as u64,
            "minStakeInternalTon": (gp.min_stake_internal / TON) as u64,
            "minStakeSensitiveTon": (gp.min_stake_sensitive / TON) as u64,
            "stakeCapTon": (gp.stake_cap / TON) as u64,
            "unbondingSecs": gp.unbonding_secs,
            "challengeWindowSecs": gp.challenge_window_secs,
            "nPublic": gp.n_public,
            "nDefault": gp.n_default,
            "nMax": gp.n_max,
            "quorum": gp.quorum,
            "checksumMin": gp.checksum_min,
            "wQualityBps": gp.w_quality_bps,
            "wStakeBps": gp.w_stake_bps,
            "wPriceBps": gp.w_price_bps,
            "attemptDeadlineMs": gp.attempt_deadline_ms,
            "progressIntervalMs": gp.progress_interval_ms,
            "progressStallMult": gp.progress_stall_mult,
            "ecoParamsCellHash": eco_cell_hash,
            "ecoParamsBocBase64": eco_cell_boc,
            "updateBodyHash": hex::encode(gp_body.cell.repr_hash()),
        },
        "opcodes": {
            "StakeVault": opgrp(&[
                ("OP_STAKE_DEPOSIT", OP_STAKE_DEPOSIT),
                ("OP_STAKE_UNBOND", OP_STAKE_UNBOND),
                ("OP_STAKE_WITHDRAW", OP_STAKE_WITHDRAW),
                ("OP_STAKE_SLASH", OP_STAKE_SLASH),
                ("OP_STAKE_ANNOUNCE_UPGRADE", OP_STAKE_ANNOUNCE_UPGRADE),
                ("OP_STAKE_APPLY_UPGRADE", OP_STAKE_APPLY_UPGRADE),
                ("OP_STAKE_CANCEL_UPGRADE", OP_STAKE_CANCEL_UPGRADE),
            ]),
            "JobEscrow": opgrp(&[
                ("OP_ESCROW_TOPUP", OP_ESCROW_TOPUP),
                ("OP_ESCROW_SETTLE", OP_ESCROW_SETTLE),
                ("OP_ESCROW_REFUND", OP_ESCROW_REFUND),
            ]),
            "RecordAnchor": opgrp(&[
                ("OP_ANCHOR_SUBMIT", OP_ANCHOR_SUBMIT),
                ("OP_ANCHOR_UPGRADE_CODE", OP_ANCHOR_UPGRADE_CODE),
            ]),
            "GlobalParams": opgrp(&[
                ("OP_UPDATE_PARAMS", OP_UPDATE_PARAMS),
                ("OP_UPDATE_ADMIN", OP_UPDATE_ADMIN),
                ("OP_UPGRADE_CODE", OP_UPGRADE_CODE),
            ]),
        },
        "escrow": ton_escrow,
        "messages": ton_messages,
    });

    // --- Assemble snapshot --------------------------------------------------
    let snapshot = json!({
        "meta": {
            "generatedAtMs": generated_at_ms,
            "generatedNote": "Generated by `cargo test -p p2p-node --test console_export -- --ignored`. Every value below is produced by the real duckdb-p2p crates running an in-process loopback-QUIC grid — no hand-authored data.",
            "protocolVersion": p2p_proto::PROTOCOL_VERSION.to_string(),
            "minSupported": p2p_proto::MIN_SUPPORTED_VERSION.to_string(),
            "wireSchemaVersion": p2p_proto::SCHEMA_VERSION,
            "engineVersion": "mock-1",
            "transport": "QUIC (Quinn + rustls), loopback",
            "tls": "TLS 1.3 · mutual auth pinned to Ed25519 node identities",
            "workspaceVersion": env!("CARGO_PKG_VERSION"),
            "buildMs": started.elapsed().as_millis() as u64,
            "jobsRun": job_recs.len() + paid_jobs.len(),
        },
        "overview": {
            "workersOnline": workers.iter().filter(|w| w.behavior != "fail").count(),
            "workersTotal": workers.len(),
            "jobsRun": job_recs.len() + paid_jobs.len(),
            "verified": verified_count,
            "failed": failed_count,
            "avgTrust": avg_trust,
            "totalStakeTon": total_stake,
            "freeMemBytes": free_mem,
            "series": series,
            "latencyHistogram": latency_hist,
            "attestationMix": attestation_mix,
        },
        "workers": workers_json,
        "jobs": jobs_json,
        "paidJobs": paid_jobs,
        "receipts": receipts_json,
        "trust": {
            "formula": "effective_trust = gate(level ≥ min_level) · clamp(α·R + β·age + γ·voucher + δ·stake − penalties) + exploration",
            "weights": {
                "alpha": weights.alpha_reputation,
                "beta": weights.beta_age,
                "gamma": weights.gamma_voucher,
                "delta": weights.delta_stake,
            },
            "minTrust": cfg.trust.min_trust,
            "bootstrapTrust": cfg.trust.bootstrap_trust,
            "halfLifeSecs": cfg.trust.reputation_half_life_secs,
            "canonical": {
                "columns": sample_rs.columns,
                "rows": sample_rs.rows.iter().map(|r| r.iter().map(value_to_string).collect::<Vec<_>>()).collect::<Vec<_>>(),
                "hash": sample_hash,
                "reorderedHash": reordered_hash,
                "orderIndependent": sample_hash == reordered_hash,
            },
            "quorum": {
                "hashes": ["h(A)","h(A)","h(A')","h(divergent)"],
                "quorum": q.quorum,
                "agreement": q.agreement,
                "agreedHash": q.agreed_hash,
                "reached": q.reached(),
            },
        },
        "settlement": {
            "enabled": paid_cfg.economics.enabled,
            "network": format!("{:?}", eco.network),
            "fees": {
                "platformFeePct": eco.fees.platform_fee_pct,
                "participationCommissionFrac": eco.fees.participation_commission_frac,
                "bonusAggressiveness": eco.fees.bonus_aggressiveness,
                "lambdaQuality": eco.fees.lambda_quality,
                "lambdaSpeed": eco.fees.lambda_speed,
                "verificationSurchargePct": eco.fees.verification_surcharge_pct,
            },
            "stake": {
                "minStake": eco.stake.min_stake,
                "minStakeInternal": eco.stake.min_stake_internal,
                "minStakeSensitive": eco.stake.min_stake_sensitive,
                "stakeCap": eco.stake.stake_cap,
                "unbondingSecs": eco.stake.unbonding_secs,
                "receiptJetton": eco.stake.receipt_jetton,
                "receiptTransferLocked": eco.stake.receipt_transfer_locked,
            },
            "slashing": {
                "wrongResultPct": eco.slashing.slash_wrong_result_pct,
                "cheatPct": eco.slashing.slash_cheat_pct,
                "downtimePct": eco.slashing.slash_downtime_pct,
                "equivocationPct": eco.slashing.slash_equivocation_pct,
                "failedCommitmentPct": eco.slashing.slash_failed_commitment_pct,
                "challengeWindowSecs": eco.slashing.challenge_window_secs,
                "toChallenger": eco.slashing.slash_to_challenger,
                "toRedundancy": eco.slashing.slash_to_redundancy,
                "toBurn": eco.slashing.slash_to_burn,
                "toTreasury": eco.slashing.slash_to_treasury,
            },
            "escrowMaxBidTon": paid_cfg.economics.pricing.max_bid,
            "events": events_json,
            "splits": split_json,
            "epochRootHex": hex::encode(epoch_root),
            "anchorRecords": anchor.len(),
            "inclusionProof": inclusion,
            "stakeTable": stake_table,
            "stakeCurve": stake_curve,
            "quality": quality,
            "binding": binding_json,
        },
        "transport": serde_json::to_value(&GridConfig::default().transport).unwrap(),
        "protocol": {
            "wire": [
                {"variant":"Hello","direction":"R↔W","purpose":"connection handshake (versions + engine build)"},
                {"variant":"VersionReject","direction":"R↔W","purpose":"typed version-incompatibility rejection"},
                {"variant":"Offer","direction":"R→W","purpose":"probe a candidate with query_hash + nonce"},
                {"variant":"Bid","direction":"W→R","purpose":"accept (ETA + attestation + receipts) or reject"},
                {"variant":"Dispatch","direction":"R→W","purpose":"full SQL + scoped credential to top-k"},
                {"variant":"Progress","direction":"W→R","purpose":"liveness heartbeat during execution"},
                {"variant":"Commit","direction":"W→R","purpose":"result_hash first (commit-first)"},
                {"variant":"Manifest","direction":"W→R","purpose":"describes result encoding/splitting"},
                {"variant":"Chunk","direction":"W→R","purpose":"bulk result bytes (winner only)"},
                {"variant":"Part","direction":"W→R","purpose":"header for one parallel stream part"},
                {"variant":"Cancel","direction":"R→W","purpose":"RESET losers"},
                {"variant":"Ack","direction":"R↔W","purpose":"generic acknowledgement / error"},
            ],
            "verdicts": [
                {"verdict":"Correct","fault":"neutral"},
                {"verdict":"Incorrect","fault":"provider"},
                {"verdict":"Timeout","fault":"provider"},
                {"verdict":"Malformed","fault":"provider"},
                {"verdict":"ResourceExceeded","fault":"requester"},
                {"verdict":"Infeasible","fault":"requester"},
                {"verdict":"Inconclusive","fault":"neutral"},
            ],
            "handshake": {
                "wireSchemaVersion": p2p_proto::SCHEMA_VERSION,
                "version": p2p_proto::PROTOCOL_VERSION,
                "minSupported": p2p_proto::MIN_SUPPORTED_VERSION,
                "requireMatchingEngineVersion": GridConfig::default().protocol.require_matching_engine_version,
            },
            "messages": protocol_messages,
        },
        "config": {
            "value": config_value,
            "exampleToml": example_toml,
        },
        "ton": {
            "computed": ton_computed,
        },
    });

    let out_dir = format!("{}/../../web/src/data", env!("CARGO_MANIFEST_DIR"));
    std::fs::create_dir_all(&out_dir).unwrap();
    let out_path = format!("{out_dir}/snapshot.json");
    std::fs::write(&out_path, serde_json::to_string_pretty(&snapshot).unwrap()).unwrap();
    println!("CONSOLE_SNAPSHOT_WRITTEN {out_path}");
    println!(
        "  workers={} jobs={} paid={} receipts={} verified={}/{} binding_ok={} epoch_records={}",
        workers.len(),
        job_recs.len(),
        paid_jobs.len(),
        receipts_json.len(),
        verified_count,
        job_recs.len(),
        binding_ok,
        anchor.len(),
    );
}

fn value_to_string(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => format!("{f}"),
        Value::Text(s) => s.clone(),
        Value::Blob(b) => format!("0x{}", hex::encode(b)),
    }
}
