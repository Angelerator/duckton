//! Live console backend: runs a REAL in-process loopback-QUIC grid (coordinator +
//! workers, real trust engine, real canonical hashing) and serves it live to the
//! web console:
//!   GET  /api/state   — current dynamic state (JSON)
//!   GET  /api/stream  — Server-Sent Events: live state on every job
//!   POST /api/query   — dispatch a REAL job; returns its outcome
//! A background loop submits ambient jobs so metrics tick continuously.
//!
//!   cargo run -p console-server        (listens on 127.0.0.1:8787)

use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::stream::{self, Stream, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value as J};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_stream::wrappers::BroadcastStream;
use tower_http::cors::CorsLayer;

use p2p_config::{
    GridConfig, IdentityConfig, PaymentPref, PinningMode, QueryOverrides, VerifyModeCfg,
};
use p2p_node::{
    AdmissionController, Candidate, Coordinator, CoordinatorError, MockEngine, QueryEngine,
    StaticDiscovery, Worker, WorkerParams,
};
use p2p_proto::{Attestation, AttestationLevel, NodeId, Value};
use p2p_settlement::types::Amount;
use p2p_settlement::{InMemoryRecordAnchor, InMemoryStakeRegistry, MockSettlement, StakeRegistry};
use p2p_transport::{NodeIdentity, QuicTransport, Transport};
use p2p_trust::{
    age_factor, exploration_bonus, now_ts, soft_trust_score, InMemoryTrustStore, TrustInputs,
    TrustStore,
};

const TON: Amount = 1_000_000_000;
const JOBS_CAP: usize = 40;
const RECEIPTS_CAP: usize = 60;
const SERIES_CAP: usize = 60;

// --------------------------------------------------------------------------
// Grid bring-up (mirrors crates/node/tests/console_export.rs)
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
    behavior: &'static str,
    _transport: Arc<QuicTransport>,
    _task: tokio::task::JoinHandle<()>,
}

struct Spec {
    alias: &'static str,
    level: AttestationLevel,
    mem_gb: u64,
    threads: u32,
    max_jobs: u32,
    delay_ms: u64,
    behavior: &'static str,
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
            evidence: b"tpm-quote".to_vec(),
            measurement: Some("known-good-boot-image".into()),
        },
        AttestationLevel::L2 => Attestation {
            level: AttestationLevel::L2,
            evidence: b"tdx-quote".to_vec(),
            measurement: Some("allowlisted-enclave-v1".into()),
        },
    }
}

async fn spawn_worker(spec: &Spec) -> WorkerHandle {
    let net = GridConfig::default().network;
    let transport =
        Arc::new(QuicTransport::bind(&net, &idcfg(), NodeIdentity::generate().unwrap()).unwrap());
    let mut budget = GridConfig::default().budget;
    budget.memory_bytes = spec.mem_gb * 1024 * 1024 * 1024;
    budget.threads = spec.threads;
    budget.max_jobs = spec.max_jobs;
    let admission = AdmissionController::new(&budget);
    let mut cfg = GridConfig::default();
    cfg.budget = budget.clone();
    let params = WorkerParams::from_config(&cfg);
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
        _transport: transport,
        _task: task,
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

/// Data-class selection policy (architecture §7.5): (min attestation tier, min trust).
/// The attestation tier is a HARD gate — Internal needs L1, Sensitive needs L2 — so
/// free/anonymous L0 nodes are refused those classes regardless of speed.
fn class_policy(data_class: &str) -> (&'static str, f64) {
    match data_class {
        "Internal" => ("L1", 0.85),
        "Sensitive" => ("L2", 0.80),
        _ => ("L0", 0.70),
    }
}

/// Free vs paid per data class (§8.2.1): the public tier runs off-chain for free;
/// Internal/Sensitive are paid, so a worker's stake counts toward its trust score
/// (the coordinator only credits stake on paid jobs).
fn class_payment(data_class: &str) -> PaymentPref {
    match data_class {
        "Internal" | "Sensitive" => PaymentPref::Paid,
        _ => PaymentPref::Free,
    }
}

// --------------------------------------------------------------------------
// Rolling accumulator + live JSON
// --------------------------------------------------------------------------

#[derive(Default)]
struct Acc {
    jobs: VecDeque<J>,
    receipts: VecDeque<J>,
    series: VecDeque<J>,
    jobs_run: u64,
    verified: u64,
    failed: u64,
    correct: BTreeMap<String, u64>,
    faults: BTreeMap<String, u64>,
    parts: BTreeMap<String, u64>,
    lats: BTreeMap<String, Vec<u64>>,
}

struct Grid {
    workers: Vec<WorkerHandle>,
    store: Arc<InMemoryTrustStore>,
    reg: Arc<InMemoryStakeRegistry>,
    coord: Coordinator,
    stakes: BTreeMap<&'static str, u64>,
    acc: Acc,
}

impl Grid {
    fn alias_of(&self, id: &NodeId) -> String {
        self.workers
            .iter()
            .find(|w| &w.node_id == id)
            .map(|w| w.alias.clone())
            .unwrap_or_else(|| "requester".into())
    }

    /// Run one real job and fold it into the accumulator. Returns the job record.
    /// When `gated`, the data-class selection policy (architecture §7.5) is applied:
    /// a minimum attestation tier + trust floor, so e.g. Internal data is refused by
    /// free/anonymous L0 nodes. Warmup jobs run ungated to build reputation.
    #[allow(clippy::too_many_arguments)]
    async fn run(
        &mut self,
        sql: &str,
        replicas: usize,
        quorum: usize,
        data_class: &str,
        verify: VerifyModeCfg,
        gated: bool,
        requester: &str,
    ) -> J {
        let (min_att, min_trust) = class_policy(data_class);
        let mut ov = QueryOverrides::default();
        ov.replicas = Some(replicas);
        ov.quorum = Some(quorum);
        ov.verify = Some(verify);
        let paid = gated && matches!(class_payment(data_class), PaymentPref::Paid);
        if gated {
            ov.min_attestation = Some(min_att.to_string());
            ov.min_trust = Some(min_trust);
            ov.payment = Some(class_payment(data_class));
        }
        let verify_label = match verify {
            VerifyModeCfg::Fast => "Fast",
            _ => "Quorum",
        };
        let policy_json = json!({ "minAttestation": min_att, "minTrust": min_trust });
        let t0 = Instant::now();
        let created = unix_ms();
        let outcome = self.coord.run_query(sql, ov).await;
        let lat = t0.elapsed().as_millis() as u64;

        let job: J = match outcome {
            Ok(o) => {
                // tally
                for r in &o.receipts {
                    let id = r.worker_id.0.clone();
                    *self.acc.parts.entry(id.clone()).or_default() += 1;
                    if r.verdict.is_correct() {
                        *self.acc.correct.entry(id.clone()).or_default() += 1;
                        self.acc.lats.entry(id).or_default().push(r.latency_ms);
                    } else if r.verdict.is_provider_fault() {
                        *self.acc.faults.entry(id).or_default() += 1;
                    }
                }
                // receipts (rolling)
                for r in &o.receipts {
                    let fault = if r.verdict.is_correct() {
                        "neutral"
                    } else if r.verdict.is_provider_fault() {
                        "provider"
                    } else {
                        "neutral"
                    };
                    self.acc.receipts.push_front(json!({
                        "jobId": r.job_id.0, "workerId": r.worker_id.0,
                        "workerAlias": self.alias_of(&r.worker_id), "requesterId": r.requester_id.0,
                        "verdict": format!("{:?}", r.verdict), "fault": fault,
                        "latencyMs": r.latency_ms, "tsMs": r.ts * 1000,
                        "resultHash": if r.result_hash.is_empty() { "—".to_string() } else { r.result_hash.clone() },
                        "sig": format!("ed25519:{}…", &r.sig.get(0..8).unwrap_or("")),
                        "verified": p2p_trust::verify_receipt(r), "gossiped": true,
                    }));
                }
                while self.acc.receipts.len() > RECEIPTS_CAP {
                    self.acc.receipts.pop_back();
                }

                // candidates
                let winner = o.winner.clone();
                let mut rs: Vec<&p2p_proto::Receipt> = o.receipts.iter().collect();
                rs.sort_by_key(|r| {
                    if r.latency_ms == 0 {
                        u64::MAX
                    } else {
                        r.latency_ms
                    }
                });
                // Terminal candidate state for the race visualization. Receipts are
                // latency-sorted, so the fastest `quorum` agreeing workers (the winner
                // + quorum-1) COMMIT and form the quorum; agreeing replicas beyond
                // quorum were hedged extras the coordinator RESET; a divergent hash is
                // committed-but-incorrect; anything else never delivered.
                let mut agree_seen = 0usize;
                let cands: Vec<J> = rs
                    .iter()
                    .map(|r| {
                        let is_winner = winner.as_ref() == Some(&r.worker_id);
                        let agreeing = o.agreed_hash.as_deref() == Some(r.result_hash.as_str())
                            && r.verdict.is_correct();
                        let agree_rank = if agreeing {
                            let i = agree_seen;
                            agree_seen += 1;
                            Some(i)
                        } else {
                            None
                        };
                        let state = if is_winner {
                            "won"
                        } else if let Some(rank) = agree_rank {
                            if rank < quorum {
                                "committed"
                            } else {
                                "reset"
                            }
                        } else if r.verdict == p2p_proto::Verdict::Incorrect {
                            "committed"
                        } else {
                            "dispatched"
                        };
                        let w = self.workers.iter().find(|w| w.node_id == r.worker_id);
                        json!({
                            "workerId": r.worker_id.0, "alias": self.alias_of(&r.worker_id),
                            "attestation": w.map(|w| w.attestation.as_str()).unwrap_or("L0"),
                            "state": state, "verdict": format!("{:?}", r.verdict),
                            "etaMs": r.latency_ms, "price": 0,
                            "progressPct": if r.verdict.is_correct() || r.verdict == p2p_proto::Verdict::Incorrect { 100 } else { 0 },
                            "committedHash": if r.result_hash.is_empty() { J::Null } else { json!(r.result_hash) },
                            "commitLatencyMs": r.latency_ms,
                        })
                    })
                    .collect();

                let mut agree_lats: Vec<u64> = o
                    .receipts
                    .iter()
                    .filter(|r| {
                        o.agreed_hash.as_deref() == Some(r.result_hash.as_str())
                            && r.verdict.is_correct()
                    })
                    .map(|r| r.latency_ms)
                    .collect();
                agree_lats.sort_unstable();
                let first_commit = agree_lats.first().copied().unwrap_or(0);
                let verify_ms = agree_lats
                    .get(quorum.saturating_sub(1))
                    .copied()
                    .unwrap_or(lat);
                let timeline = json!([
                    {"tMs":0,"stage":"offer","label":format!("Offer broadcast to {} candidates", o.receipts.len()),"detail":"query_hash + nonce"},
                    {"tMs":2,"stage":"bidding","label":"Bids collected","detail":"top-k by trust + ETA"},
                    {"tMs":4,"stage":"dispatch","label":"Dispatch SQL to top-k","detail":""},
                    {"tMs":first_commit,"stage":"commit","label":"First result_hash committed","detail":"commit-first"},
                    {"tMs":verify_ms,"stage":"verify","label":format!("Quorum {}/{}", o.agreement, o.quorum),"detail":o.agreed_hash.clone()},
                    {"tMs":lat,"stage":"settle","label": if o.verified {"Winner streams · losers RESET"} else {"Failed — re-dispatch"},"detail":""},
                ]);

                if o.verified {
                    self.acc.verified += 1;
                } else {
                    self.acc.failed += 1;
                }

                json!({
                    "id": o.job_id.0, "sql": sql, "fn": "p2p_query",
                    "dataClass": data_class, "verifyMode": verify_label, "policy": policy_json.clone(),
                    "quorum": o.quorum, "k": o.receipts.len(),
                    "status": if o.verified { "verified" } else { "failed" },
                    "paid": paid, "requester": requester, "createdAtMs": created,
                    "rowCount": o.result.row_count(), "resultHash": o.agreed_hash,
                    "latencyMs": lat, "escrowTon": if paid { 100 } else { 0 },
                    "winner": winner.as_ref().map(|w| self.alias_of(w)),
                    "winnerId": winner.as_ref().map(|w| w.0.clone()),
                    "source": "live in-process grid (loopback)",
                    "candidates": cands, "timeline": timeline,
                    "result": { "columns": o.result.columns, "rows": o.result.rows.iter().take(8).map(|row| row.iter().map(value_to_string).collect::<Vec<_>>()).collect::<Vec<_>>() },
                })
            }
            Err(e) => {
                self.acc.failed += 1;
                // A failed query dispatched/settled nothing (k=0, no winner): no
                // escrow was locked and no payment occurred, so report `paid:false`
                // + `escrowTon:0` regardless of the requested payment mode.
                //
                // Distinguish the two `InsufficientWorkers` shapes (it carries how
                // many hosts cleared the policy vs the quorum needed): "0 eligible"
                // is a policy/availability problem, whereas "quorum exceeds the
                // eligible set" means hosts DO qualify but too few for this quorum.
                let error_msg = match &e {
                    CoordinatorError::InsufficientWorkers { have, quorum } => {
                        if *have == 0 {
                            format!("No hosts meet the {data_class} policy (≥ {min_att} attestation, trust ≥ {min_trust})")
                        } else {
                            format!("Only {have} host(s) meet the {data_class} policy (≥ {min_att} attestation, trust ≥ {min_trust}), but quorum is {quorum}")
                        }
                    }
                    other => format!("{other:?}"),
                };
                json!({
                    "id": format!("job_err_{}", created), "sql": sql, "fn": "p2p_query",
                    "dataClass": data_class, "verifyMode": verify_label, "policy": policy_json,
                    "quorum": quorum, "k": 0,
                    "status": "failed", "paid": false, "requester": requester, "createdAtMs": created,
                    "rowCount": 0, "resultHash": J::Null, "latencyMs": lat, "escrowTon": 0,
                    "winner": J::Null, "winnerId": J::Null, "source": "live in-process grid",
                    "candidates": [], "timeline": [], "result": {"columns": [], "rows": []},
                    "error": error_msg,
                })
            }
        };

        self.acc.jobs_run += 1;
        self.acc.seq_push_series(&job);
        self.acc.jobs.push_front(job.clone());
        while self.acc.jobs.len() > JOBS_CAP {
            self.acc.jobs.pop_back();
        }
        job
    }

    fn live_json(&self) -> J {
        let now = now_ts();
        let cfg = GridConfig::default();
        let eco = cfg.economics;
        let weights = &cfg.trust.weights;

        let workers: Vec<J> = self
            .workers
            .iter()
            .map(|w| {
                let id = w.node_id.0.clone();
                let obs = self.store.observation_count(&w.node_id);
                let rep_raw = self.store.reputation(&w.node_id, now);
                let rep_conf = self
                    .store
                    .confident_reputation(&w.node_id, now, eco.reputation.prior_alpha, eco.reputation.prior_beta, eco.reputation.confidence_z)
                    .unwrap_or(cfg.trust.bootstrap_trust);
                let vouch = self.store.voucher_trust(&w.node_id).min(1.0);
                let penalty = self.store.penalty(&w.node_id);
                let sf = self.reg.stake_factor(&w.node_id);
                let inputs = TrustInputs {
                    reputation: rep_conf,
                    age_factor: age_factor(obs, 20),
                    voucher_trust: vouch,
                    stake_factor: sf,
                    penalties: penalty,
                };
                let soft = soft_trust_score(weights, &inputs);
                let expl = exploration_bonus(obs, eco.ranking.exploration_rate, eco.ranking.exploration_saturation);
                let effective = (soft + expl).clamp(0.0, 1.0);
                let c = *self.acc.correct.get(&id).unwrap_or(&0);
                let f = *self.acc.faults.get(&id).unwrap_or(&0);
                let success = if c + f == 0 { 1.0 } else { c as f64 / (c + f) as f64 };
                let p50 = median(self.acc.lats.get(&id).cloned().unwrap_or_default());
                let stake = self.stakes.get(w.alias.as_str()).copied().unwrap_or(0);
                json!({
                    "id": id, "alias": w.alias, "attestation": w.attestation.as_str(),
                    "behavior": w.behavior, "trust": effective, "soft": soft,
                    "reputation": rep_raw, "reputationConfident": rep_conf, "observations": obs,
                    "ageFactor": age_factor(obs, 20), "voucherTrust": vouch, "stakeFactor": sf,
                    "penalty": penalty, "explorationBonus": expl, "stakeTon": stake,
                    "stakeNanoton": (stake as Amount * TON).to_string(),
                    "totalMemBytes": w.budget_mem, "totalThreads": w.budget_threads, "maxJobs": w.max_jobs,
                    "jobsParticipated": *self.acc.parts.get(&id).unwrap_or(&0),
                    "correct": c, "faults": f, "successRate": success, "p50LatencyMs": p50,
                    "delayMs": w.delay_ms, "online": w.behavior != "fail", "engineVersion": "mock-1",
                    "wallet": if stake > 0 { Some(format!("0:{}", hex::encode(blake3::hash(id.as_bytes()).as_bytes()))) } else { None },
                })
            })
            .collect();

        // latency histogram from rolling correct receipt latencies
        let blabels = ["<25ms", "25–50", "50–100", "100–250", "250ms–1s", ">1s"];
        let mut buckets = [0u64; 6];
        for r in &self.acc.receipts {
            if r["verdict"] == "Correct" {
                let l = r["latencyMs"].as_u64().unwrap_or(0);
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

        let mut att: BTreeMap<&str, u64> = BTreeMap::new();
        for w in &self.workers {
            *att.entry(w.attestation.as_str()).or_default() += 1;
        }
        let att_fill = |l: &str| match l {
            "L2" => "var(--chart-1)",
            "L1" => "var(--chart-2)",
            _ => "var(--chart-3)",
        };
        let attestation_mix: Vec<J> = ["L0", "L1", "L2"].iter().map(|l| json!({
            "level": format!("{} — {}", l, match *l {"L2"=>"TEE enclave","L1"=>"measured boot",_=>"anonymous"}),
            "count": att.get(l).copied().unwrap_or(0), "fill": att_fill(l)
        })).collect();

        let avg_trust = {
            let v: Vec<f64> = workers
                .iter()
                .map(|w| w["trust"].as_f64().unwrap_or(0.0))
                .collect();
            if v.is_empty() {
                0.0
            } else {
                v.iter().sum::<f64>() / v.len() as f64
            }
        };
        let total_stake: u64 = self.stakes.values().sum();
        let free_mem: u64 = self.workers.iter().map(|w| w.budget_mem).sum();

        // comm graph from rolling jobs (quorum) + receipts (dispatch)
        let comm = self.comm_graph();

        json!({
            "meta": { "live": true, "ts": unix_ms(), "jobsRun": self.acc.jobs_run, "engineVersion": "mock-1", "transport": "QUIC (Quinn+rustls), loopback" },
            "overview": {
                "workersOnline": self.workers.iter().filter(|w| w.behavior != "fail").count(),
                "workersTotal": self.workers.len(),
                "jobsRun": self.acc.jobs_run, "verified": self.acc.verified, "failed": self.acc.failed,
                "avgTrust": avg_trust, "totalStakeTon": total_stake, "freeMemBytes": free_mem,
                "series": self.acc.series.iter().cloned().collect::<Vec<_>>(),
                "latencyHistogram": latency_hist, "attestationMix": attestation_mix,
            },
            "workers": workers,
            "jobs": self.acc.jobs.iter().cloned().collect::<Vec<_>>(),
            "receipts": self.acc.receipts.iter().cloned().collect::<Vec<_>>(),
            "commGraph": comm,
        })
    }

    fn comm_graph(&self) -> J {
        use std::collections::HashMap;
        let mut nodes: HashMap<String, J> = HashMap::new();
        let mut degree: HashMap<String, u64> = HashMap::new();
        for w in &self.workers {
            let group = if w.behavior == "honest" {
                "worker"
            } else {
                w.behavior
            };
            nodes.insert(w.node_id.0.clone(), json!({"id": w.node_id.0, "label": w.alias, "group": group, "degree": 0, "trust": 0.0}));
        }
        let mut edges: HashMap<String, J> = HashMap::new();
        let mut bump = |a: &str, b: &str, kind: &str, deg: &mut HashMap<String, u64>| {
            let key = if kind == "quorum" && a > b {
                format!("{b}|{a}|{kind}")
            } else {
                format!("{a}|{b}|{kind}")
            };
            let e = edges
                .entry(key)
                .or_insert_with(|| json!({"source": a, "target": b, "weight": 0, "kind": kind}));
            e["weight"] = json!(e["weight"].as_u64().unwrap_or(0) + 1);
            *deg.entry(a.to_string()).or_default() += 1;
            *deg.entry(b.to_string()).or_default() += 1;
        };
        let mut req_idx = 0u64;
        let mut req_label: HashMap<String, String> = HashMap::new();
        for r in &self.acc.receipts {
            let rid = r["requesterId"].as_str().unwrap_or("").to_string();
            let wid = r["workerId"].as_str().unwrap_or("").to_string();
            if rid.is_empty() || wid.is_empty() {
                continue;
            }
            if !req_label.contains_key(&rid) {
                req_idx += 1;
                req_label.insert(rid.clone(), format!("requester-{req_idx}"));
                nodes.entry(rid.clone()).or_insert_with(|| json!({"id": rid, "label": format!("requester-{req_idx}"), "group": "requester", "degree": 0, "trust": 1.0}));
            }
            bump(&rid, &wid, "dispatch", &mut degree);
        }
        for j in &self.acc.jobs {
            if let Some(cands) = j["candidates"].as_array() {
                let agree: Vec<String> = cands
                    .iter()
                    .filter(|c| c["verdict"] == "Correct")
                    .filter_map(|c| c["workerId"].as_str().map(String::from))
                    .collect();
                for a in 0..agree.len() {
                    for b in (a + 1)..agree.len() {
                        bump(&agree[a], &agree[b], "quorum", &mut degree);
                    }
                }
            }
        }
        for (id, n) in nodes.iter_mut() {
            n["degree"] = json!(degree.get(id).copied().unwrap_or(0));
        }
        json!({ "nodes": nodes.into_values().collect::<Vec<_>>(), "edges": edges.into_values().collect::<Vec<_>>() })
    }
}

impl Acc {
    fn seq_push_series(&mut self, job: &J) {
        self.seq_inc();
        self.series.push_back(json!({
            "label": format!("j{}", self.jobs_run),
            "latencyMs": job["latencyMs"].as_u64().unwrap_or(0),
            "verified": if job["status"] == "verified" { 1 } else { 0 },
        }));
        while self.series.len() > SERIES_CAP {
            self.series.pop_front();
        }
    }
    fn seq_inc(&mut self) {}
}

// --------------------------------------------------------------------------
// HTTP
// --------------------------------------------------------------------------

#[derive(Clone)]
struct AppState {
    q_tx: mpsc::Sender<QueryReq>,
    bcast: broadcast::Sender<String>,
    latest: Arc<Mutex<J>>,
}

struct QueryReq {
    sql: String,
    replicas: usize,
    quorum: usize,
    data_class: String,
    verify: VerifyModeCfg,
    gated: bool,
    requester: String,
    resp: Option<oneshot::Sender<J>>,
}

#[derive(Deserialize)]
struct QueryBody {
    #[serde(default)]
    sql: Option<String>,
    #[serde(default, rename = "dataClass")]
    data_class: Option<String>,
    #[serde(default, rename = "verifyMode")]
    verify_mode: Option<String>,
    #[serde(default)]
    quorum: Option<usize>,
    #[serde(default)]
    k: Option<usize>,
}

async fn state_handler(State(s): State<AppState>) -> Json<J> {
    Json(s.latest.lock().unwrap().clone())
}

async fn stream_handler(
    State(s): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let init = s.latest.lock().unwrap().to_string();
    let rx = s.bcast.subscribe();
    let head = stream::once(async move { Ok(Event::default().data(init)) });
    let tail = BroadcastStream::new(rx)
        .filter_map(|r| async move { r.ok().map(|d| Ok(Event::default().data(d))) });
    Sse::new(head.chain(tail)).keep_alive(KeepAlive::default())
}

async fn query_handler(State(s): State<AppState>, Json(body): Json<QueryBody>) -> Json<J> {
    let sql = body
        .sql
        .unwrap_or_else(|| "SELECT region, count(*) FROM orders GROUP BY region".into());
    let k = body.k.unwrap_or(6).clamp(1, 8);
    let quorum = body.quorum.unwrap_or(3).clamp(1, k);
    let verify = match body.verify_mode.as_deref() {
        Some("Fast") => VerifyModeCfg::Fast,
        _ => VerifyModeCfg::Quorum,
    };
    let (tx, rx) = oneshot::channel();
    let req = QueryReq {
        sql,
        replicas: k,
        quorum,
        data_class: body.data_class.unwrap_or_else(|| "Public".into()),
        verify,
        gated: true,
        requester: "you".into(),
        resp: Some(tx),
    };
    if s.q_tx.send(req).await.is_err() {
        return Json(json!({"error": "grid offline"}));
    }
    Json(rx.await.unwrap_or_else(|_| json!({"error": "grid busy"})))
}

// --------------------------------------------------------------------------

const AMBIENT_SQL: &[&str] = &[
    "SELECT region, count(*) AS orders, sum(total) AS gmv FROM orders GROUP BY region",
    "SELECT date_trunc('hour', ts) h, avg(latency_ms) FROM telemetry GROUP BY 1",
    "SELECT sku, sum(qty) FROM sales GROUP BY sku HAVING sum(qty) > 1000",
    "SELECT cohort, count(DISTINCT user_id) FROM events WHERE kind='session' GROUP BY cohort",
    "SELECT country, sum(revenue) FROM invoices GROUP BY country ORDER BY 2 DESC LIMIT 20",
    "SELECT model, avg(score) FROM predictions GROUP BY model",
    "SELECT status, count(*) FROM jobs GROUP BY status",
    "SELECT tier, sum(amount) FROM payments GROUP BY tier",
];

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let specs = [
        Spec {
            alias: "frost-owl",
            level: AttestationLevel::L2,
            mem_gb: 64,
            threads: 32,
            max_jobs: 12,
            delay_ms: 12,
            behavior: "honest",
        },
        Spec {
            alias: "harbor-vole",
            level: AttestationLevel::L2,
            mem_gb: 96,
            threads: 40,
            max_jobs: 10,
            delay_ms: 22,
            behavior: "honest",
        },
        Spec {
            alias: "tidal-fox",
            level: AttestationLevel::L2,
            mem_gb: 32,
            threads: 16,
            max_jobs: 6,
            delay_ms: 30,
            behavior: "honest",
        },
        Spec {
            alias: "marsh-otter",
            level: AttestationLevel::L2,
            mem_gb: 48,
            threads: 24,
            max_jobs: 8,
            delay_ms: 18,
            behavior: "honest",
        },
        Spec {
            alias: "amber-mole",
            level: AttestationLevel::L1,
            mem_gb: 24,
            threads: 12,
            max_jobs: 4,
            delay_ms: 45,
            behavior: "honest",
        },
        Spec {
            alias: "pine-marten",
            level: AttestationLevel::L1,
            mem_gb: 16,
            threads: 8,
            max_jobs: 4,
            delay_ms: 60,
            behavior: "honest",
        },
        Spec {
            alias: "slate-heron",
            level: AttestationLevel::L0,
            mem_gb: 8,
            threads: 4,
            max_jobs: 2,
            delay_ms: 110,
            behavior: "honest",
        },
        Spec {
            alias: "rust-shrike",
            level: AttestationLevel::L0,
            mem_gb: 8,
            threads: 4,
            max_jobs: 2,
            delay_ms: 18,
            behavior: "cheat",
        },
        Spec {
            alias: "cobalt-stoat",
            level: AttestationLevel::L0,
            mem_gb: 4,
            threads: 2,
            max_jobs: 1,
            delay_ms: 25,
            behavior: "fail",
        },
    ];
    let mut workers = Vec::new();
    for s in &specs {
        workers.push(spawn_worker(s).await);
    }
    let store = Arc::new(InMemoryTrustStore::new(
        &GridConfig::default().trust,
        &GridConfig::default().limits,
    ));
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

    let eco = GridConfig::default().economics;
    let reg = Arc::new(InMemoryStakeRegistry::from_config(&eco.stake));
    // Honest nodes are staked so their effective trust clears the per-class floor
    // after warmup; cheat/fail nodes earn no stake and sink below every floor.
    let stakes: BTreeMap<&'static str, u64> = [
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
        if let Some(t) = stakes.get(w.alias.as_str()) {
            reg.set_stake(&w.node_id, *t as Amount * TON);
        }
    }

    // Coordinator over a static view of all workers.
    let mut c = GridConfig::default();
    c.scheduler.replicas = 6;
    c.scheduler.quorum = 3;
    c.scheduler.offer_timeout_ms = 2_000;
    c.scheduler.dispatch_timeout_ms = 6_000;
    c.scheduler.attempt_deadline_ms = 12_000;
    c.trust.min_trust = 0.0; // base gate; per-query overrides apply the class policy
    c.discovery.candidate_sample_size = 32;
    // Enable the economic layer so paid (Internal/Sensitive) jobs credit stake toward
    // trust; the per-query `payment` override decides free vs paid.
    c.economics.enabled = true;
    c.economics.pricing.max_bid = 100;
    let cfg = Arc::new(c);
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
    let coord = Coordinator::new(req, disc, store.clone(), cfg, "mock-1")
        .with_stake_registry(reg.clone())
        .with_settlement(Arc::new(MockSettlement::new()))
        .with_record_anchor(Arc::new(InMemoryRecordAnchor::new()));

    let mut grid = Grid {
        workers,
        store,
        reg,
        coord,
        stakes,
        acc: Acc::default(),
    };

    // Warm up: run ungated jobs so honest nodes accrue reputation (and the cheat/
    // fail nodes sink) before the data-class trust floors take effect — otherwise a
    // cold grid would have nobody meeting the Internal/Sensitive floor.
    // quorum 6 (= honest-node count) forces every honest node to commit each job,
    // so they all accrue reputation — otherwise only the ~3 fastest would, and the
    // slower honest nodes couldn't clear the Internal trust floor.
    println!("warming up grid (building reputation)…");
    for i in 0..60usize {
        grid.run(
            AMBIENT_SQL[i % AMBIENT_SQL.len()],
            8,
            6,
            "Public",
            VerifyModeCfg::Quorum,
            false,
            "warmup",
        )
        .await;
    }

    let (q_tx, mut q_rx) = mpsc::channel::<QueryReq>(64);
    let (bcast, _) = broadcast::channel::<String>(64);
    let latest = Arc::new(Mutex::new(grid.live_json()));

    // Grid task: owns the grid, processes queries sequentially, broadcasts state.
    {
        let bcast = bcast.clone();
        let latest = latest.clone();
        tokio::spawn(async move {
            while let Some(reqq) = q_rx.recv().await {
                let job = grid
                    .run(
                        &reqq.sql,
                        reqq.replicas,
                        reqq.quorum,
                        &reqq.data_class,
                        reqq.verify,
                        reqq.gated,
                        &reqq.requester,
                    )
                    .await;
                let state = grid.live_json();
                *latest.lock().unwrap() = state.clone();
                let _ = bcast.send(state.to_string());
                if let Some(resp) = reqq.resp {
                    let _ = resp.send(job);
                }
            }
        });
    }

    // Ambient job generator → continuous liveness.
    {
        let q_tx = q_tx.clone();
        tokio::spawn(async move {
            let mut i = 0usize;
            let mut tick = tokio::time::interval(Duration::from_millis(2500));
            loop {
                tick.tick().await;
                let sql = AMBIENT_SQL[i % AMBIENT_SQL.len()];
                i += 1;
                let _ = q_tx
                    .send(QueryReq {
                        sql: sql.into(),
                        replicas: 6,
                        quorum: 3,
                        data_class: "Public".into(),
                        verify: VerifyModeCfg::Quorum,
                        gated: true,
                        requester: "ambient".into(),
                        resp: None,
                    })
                    .await;
            }
        });
    }

    let app = Router::new()
        .route("/api/state", get(state_handler))
        .route("/api/stream", get(stream_handler))
        .route("/api/query", post(query_handler))
        .route("/api/health", get(|| async { "ok" }))
        .layer(CorsLayer::very_permissive())
        .with_state(AppState {
            q_tx,
            bcast,
            latest,
        });

    let addr: SocketAddr = "127.0.0.1:8787".parse().unwrap();
    println!(
        "console-server live grid → http://{addr}  (SSE: /api/stream, query: POST /api/query)"
    );
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
