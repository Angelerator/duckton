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
    GridConfig, IdentityConfig, PaymentPref, PinningMode, PreferMode, QueryOverrides, VerifyModeCfg,
};
use p2p_node::{
    AdmissionController, Candidate, Coordinator, CoordinatorError, DuckDbEngine, MockEngine,
    QueryEngine, StaticDiscovery, StorageSetup, Worker, WorkerParams,
};
use p2p_proto::{Attestation, AttestationLevel, NodeId, Value};
use p2p_settlement::types::Amount;
use p2p_settlement::{InMemoryRecordAnchor, InMemoryStakeRegistry, MockSettlement, StakeRegistry};
use p2p_transport::{NodeIdentity, QuicTransport, Transport};
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;

use p2p_trust::{
    age_factor, exploration_bonus, now_ts, soft_trust_score, AllowlistVerifier, InMemoryTrustStore,
    MockAttestor, TrustInputs, TrustStore,
};

const TON: Amount = 1_000_000_000;
const JOBS_CAP: usize = 40;
const RECEIPTS_CAP: usize = 60;
const SERIES_CAP: usize = 60;
/// Default escrow bound `B` when the caller gives no `maxEscrow`: the computed
/// job cost scaled by a small safety margin, so the lock covers the payout with
/// slack to refund — NOT a flat 100 TON.
const ESCROW_SAFETY_FACTOR: f64 = 1.5;

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
    /// Advertised unit price (whole TON) this host bids on a paid job. The
    /// winning host's price is the base reward the escrow settles (see
    /// [`settle_escrow`]); a trivial query costs ~this, NOT a flat 100.
    price: u64,
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
    /// Advertised unit price (whole TON) — see [`WorkerHandle::price`].
    price: u64,
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

/// Demo attestation key binding (console-server DEMO only): the mock attestor
/// binds this fixed key into its per-offer evidence and the verifier checks it
/// matches. Production nodes ship no attestor/verifier.
const DEMO_BOUND_PUB: [u8; 32] = [0xBD; 32];

/// The allowlisted measurement a demo host at `level` attests to (matches the
/// `attest()` stub measurements). L0 has none (anonymous, no attestor).
fn demo_measurement(level: AttestationLevel) -> &'static str {
    match level {
        AttestationLevel::L2 => "allowlisted-enclave-v1",
        AttestationLevel::L1 => "known-good-boot-image",
        AttestationLevel::L0 => "",
    }
}

/// Storage setup for an honest host's REAL DuckDB engine: keeps the default
/// lockdown (no network egress, no `INSTALL`/`LOAD`, local FS disabled) but, when
/// `~/tpch-data` exists, additionally allow-lists it so file-backed demos like
/// `read_parquet('<home>/tpch-data/sf1/lineitem.parquet')` succeed. Pure
/// in-memory/compute SQL (`SELECT 42`, `range(5)`, …) needs none of this.
fn honest_storage_setup() -> StorageSetup {
    let mut storage = GridConfig::default().storage;
    if let Ok(home) = std::env::var("HOME") {
        let tpch = format!("{home}/tpch-data");
        if std::path::Path::new(&tpch).is_dir() {
            storage.allowed_local_paths = vec![tpch];
        }
    }
    StorageSetup::from_config(&storage)
}

async fn spawn_worker(spec: &Spec, attestor_authority: &SigningKey) -> WorkerHandle {
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
    // HONEST hosts run the REAL locked-down DuckDB engine, so dispatched SQL is
    // actually executed and the true columns/rows are returned + canonical-hashed.
    // CHEAT/FAIL hosts stay on the Mock engine so the trust/quorum/equivocation
    // demo still works (the cheater returns a divergent hash and is caught; the
    // failing node errors; both lose to the honest quorum on the real result).
    let engine: Arc<dyn QueryEngine> = match spec.behavior {
        "cheat" => Arc::new(
            MockEngine::deterministic()
                .cheating()
                .with_delay(Duration::from_millis(spec.delay_ms)),
        ),
        "fail" => Arc::new(
            MockEngine::failing("simulated worker fault")
                .with_delay(Duration::from_millis(spec.delay_ms)),
        ),
        _ => Arc::new(
            DuckDbEngine::with_setup(honest_storage_setup())
                .expect("DuckDb engine init (honest demo host)"),
        ),
    };
    let node_id = transport.local_node_id().clone();
    let addr = transport.local_addr().unwrap();
    let mut worker = Worker::new(
        transport.clone(),
        engine,
        admission,
        attest(spec.level),
        params,
    );
    // Demo honor-path (console-server DEMO only): L1/L2 hosts carry a MockAttestor
    // signed by the shared demo authority, so they emit REAL per-offer, nonce-bound
    // evidence that the coordinator's AllowlistVerifier checks — making the
    // attestation gate genuine rather than a spoofable integer compare. L0 hosts
    // stay anonymous (no attestor). Production nodes ship NO attestor.
    if spec.level != AttestationLevel::L0 {
        let attestor = Arc::new(MockAttestor::new(
            attestor_authority.clone(),
            demo_measurement(spec.level),
            spec.level,
        ));
        worker = worker.with_attestor(attestor, DEMO_BOUND_PUB);
    }
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

/// The escrow settlement breakdown for one paid job (all in whole TON).
struct EscrowSettlement {
    /// Actual job cost = winner base + platform fee + participation commissions.
    cost: f64,
    /// Escrow bound `B` actually locked (the cap).
    cap: f64,
    /// Released to providers + treasury (the FULL `cost` when covered, else 0).
    settled: f64,
    /// Returned to the requester (`B − settled`).
    refunded: f64,
    /// Whether the locked escrow `B` actually covers the full multi-party cost
    /// (winner + 15% platform fee + 5% commission × N runners). `false` ⇒ the job
    /// is rejected up front (nothing settled); we NEVER silently under-pay.
    covered: bool,
    /// Human-readable up-front rejection reason when `!covered` (else empty).
    coverage_error: String,
}

/// Compute the real escrow settlement for a paid job (§10.1 payout shape) — the
/// fix for the bug where every paid job hardcoded `escrowTon: 100`.
///
/// * `base` — the winning host's advertised bid price (the winner base reward).
/// * `agreeing_non_winners` — agreeing replicas other than the winner, each paid
///   a fixed participation commission `κ·base`.
/// * `fee_pct` / `commission_frac` — the configured platform fee φ and κ.
/// * `max_escrow` — the caller's `maxEscrow` cap if any; else the cost is bounded
///   by [`ESCROW_SAFETY_FACTOR`].
///
/// UP-FRONT COVERAGE (no silent under-pay): the cost MUST be coverable by the
/// locked escrow `B` (= `cap`). The check reuses the shared, integer-exact
/// `p2p_settlement::ensure_escrow_covers` (nanoton) so the demo, the off-chain
/// coordinator preflight, and the on-chain `JobEscrow` bound all agree. When the
/// escrow cannot cover ALL parties the job is REJECTED (`covered = false`, nothing
/// settled, the full `B` refunded) with a human-readable reason — settlement is
/// all-or-nothing. Otherwise `settled = cost` and `refunded = B − cost`.
fn settle_escrow(
    base: f64,
    agreeing_non_winners: usize,
    fee_pct: f64,
    commission_frac: f64,
    max_escrow: Option<f64>,
) -> EscrowSettlement {
    let round = |x: f64| (x * 1e4).round() / 1e4;
    let platform_fee = base * fee_pct;
    let commissions = base * commission_frac * agreeing_non_winners as f64;
    let cost = round(base + platform_fee + commissions);
    let cap = round(match max_escrow {
        Some(m) if m > 0.0 => m,
        _ => cost * ESCROW_SAFETY_FACTOR,
    });
    // Shared coverage check (nanoton, integer-exact) — the single source of truth.
    const TON: f64 = 1e9;
    let to_nano = |x: f64| (x * TON).round().max(0.0) as p2p_settlement::Amount;
    let fee_bps = (fee_pct * 10_000.0).round().clamp(0.0, 65_535.0) as u16;
    let comm_bps = (commission_frac * 10_000.0).round().clamp(0.0, 65_535.0) as u16;
    let (covered, coverage_error) = match p2p_settlement::ensure_escrow_covers(
        to_nano(cap),
        to_nano(base),
        agreeing_non_winners,
        fee_bps,
        comm_bps,
    ) {
        Ok(_) => (true, String::new()),
        Err(e) => (false, e.to_string()),
    };
    let settled = if covered { cost } else { 0.0 };
    let refunded = round(cap - settled);
    EscrowSettlement {
        cost,
        cap,
        settled,
        refunded,
        covered,
        coverage_error,
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
    /// Platform fee fraction and participation commission κ from the active
    /// economics config — drive the paid-job escrow settlement cost model.
    fee_pct: f64,
    commission_frac: f64,
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
        extras: QueryExtras,
    ) -> J {
        let max_escrow = extras.max_escrow;
        let (min_att, min_trust) = class_policy(data_class);
        let mut ov = QueryOverrides::default();
        ov.replicas = Some(replicas);
        ov.quorum = Some(quorum);
        ov.verify = Some(verify);
        if gated {
            ov.min_attestation = Some(min_att.to_string());
            ov.min_trust = Some(min_trust);
            ov.payment = Some(class_payment(data_class));
        }
        // Explicit per-call overrides (highest precedence) — set when provided,
        // mirroring how `QueryOverrides::apply` lets per-call values win. Omitted
        // fields keep the data-class policy floor / current behavior.
        if let Some(p) = extras.prefer {
            ov.prefer = Some(p);
        }
        if let Some(p) = extras.payment {
            ov.payment = Some(p);
        }
        if let Some(t) = extras.min_trust {
            ov.min_trust = Some(t);
        }
        if let Some(a) = &extras.min_attestation {
            ov.min_attestation = Some(a.clone());
        }
        if let Some(n) = &extras.network {
            ov.network = Some(n.clone());
        }
        if let Some(g) = &extras.groups {
            ov.groups = g.clone();
        }
        if let Some(r) = &extras.regions {
            ov.regions = r.clone();
        }
        if let Some(b) = extras.require_staked_hosts {
            ov.require_staked_hosts = Some(b);
        }
        let paid = matches!(ov.payment, Some(PaymentPref::Paid));
        // Effective policy actually applied to selection (floor or override).
        let eff_min_att = ov
            .min_attestation
            .clone()
            .unwrap_or_else(|| min_att.to_string());
        let eff_min_trust = ov.min_trust.unwrap_or(min_trust);
        let verify_label = match verify {
            VerifyModeCfg::Fast => "Fast",
            _ => "Quorum",
        };
        let policy_json = json!({ "minAttestation": eff_min_att, "minTrust": eff_min_trust });
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
                            "etaMs": r.latency_ms, "price": w.map(|w| w.price).unwrap_or(0),
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

                // Real escrow settlement (replaces the hardcoded `escrowTon: 100`).
                // Only a PAID job that actually produced a winner settles; the cost
                // is the winning host's bid price + platform fee + a participation
                // commission to each agreeing non-winner, bounded by the escrow cap.
                let settle = if paid {
                    winner.as_ref().and_then(|w| {
                        self.workers.iter().find(|x| &x.node_id == w).map(|x| {
                            settle_escrow(
                                x.price as f64,
                                o.agreement.saturating_sub(1),
                                self.fee_pct,
                                self.commission_frac,
                                max_escrow,
                            )
                        })
                    })
                } else {
                    None
                };
                let (settled_ton, escrow_cap_ton, refunded_ton) = settle
                    .as_ref()
                    .map(|s| (s.settled, s.cap, s.refunded))
                    .unwrap_or((0.0, 0.0, 0.0));

                json!({
                    "id": o.job_id.0, "sql": sql, "fn": "p2p_query",
                    "dataClass": data_class, "verifyMode": verify_label, "policy": policy_json.clone(),
                    "quorum": o.quorum, "k": o.receipts.len(),
                    "status": if o.verified { "verified" } else { "failed" },
                    "paid": paid, "requester": requester, "createdAtMs": created,
                    "rowCount": o.result.row_count(), "resultHash": o.agreed_hash,
                    "latencyMs": lat,
                    // Truthful settlement numbers. `escrowTon` retained (= the cap B)
                    // for back-compat with the current web display; the web will
                    // switch to settledTon/refundedTon separately.
                    "escrowTon": escrow_cap_ton,
                    "settledTon": settled_ton,
                    "escrowCapTon": escrow_cap_ton,
                    "refundedTon": refunded_ton,
                    "costTon": settle.as_ref().map(|s| s.cost).unwrap_or(0.0),
                    // Up-front coverage: false ⇒ the escrow could not cover winner +
                    // 15% platform fee + 5% commission × N runners, so the job was
                    // rejected (nothing settled) with a human-readable reason.
                    "escrowCovered": settle.as_ref().map(|s| s.covered).unwrap_or(true),
                    "escrowCoverageError": settle.as_ref().map(|s| s.coverage_error.clone()).unwrap_or_default(),
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
                            format!("No hosts meet the {data_class} policy (≥ {eff_min_att} attestation, trust ≥ {eff_min_trust})")
                        } else {
                            format!("Only {have} host(s) meet the {data_class} policy (≥ {eff_min_att} attestation, trust ≥ {eff_min_trust}), but quorum is {quorum}")
                        }
                    }
                    // Use the human Display text (thiserror `#[error(...)]`), not
                    // the Rust `{:?}` Debug wrapper — so the requester sees a clean
                    // reason (now carrying the real underlying engine message, e.g.
                    // `… : Binder Error: Referenced column "alireza" not found`).
                    other => other.to_string(),
                };
                json!({
                    "id": format!("job_err_{}", created), "sql": sql, "fn": "p2p_query",
                    "dataClass": data_class, "verifyMode": verify_label, "policy": policy_json,
                    "quorum": quorum, "k": 0,
                    "status": "failed", "paid": false, "requester": requester, "createdAtMs": created,
                    "rowCount": 0, "resultHash": J::Null, "latencyMs": lat,
                    "escrowTon": 0, "settledTon": 0, "escrowCapTon": 0, "refundedTon": 0, "costTon": 0,
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
    extras: QueryExtras,
    resp: Option<oneshot::Sender<J>>,
}

/// Optional per-call overrides from `/api/query` (all default to None ⇒ current
/// behavior). These map onto the corresponding [`QueryOverrides`] fields and are
/// applied with HIGHEST precedence (after the data-class policy floor).
#[derive(Default)]
struct QueryExtras {
    prefer: Option<PreferMode>,
    payment: Option<PaymentPref>,
    min_trust: Option<f64>,
    min_attestation: Option<String>,
    network: Option<String>,
    groups: Option<Vec<String>>,
    regions: Option<Vec<String>>,
    require_staked_hosts: Option<bool>,
    /// Per-call escrow cap `B` (whole TON). `None` ⇒ default to the job cost
    /// scaled by [`ESCROW_SAFETY_FACTOR`].
    max_escrow: Option<f64>,
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
    // --- Full override set (camelCase). All optional. ---
    #[serde(default)]
    prefer: Option<String>,
    #[serde(default)]
    payment: Option<String>,
    #[serde(default, rename = "minTrust")]
    min_trust: Option<f64>,
    #[serde(default, rename = "minAttestation")]
    min_attestation: Option<String>,
    #[serde(default)]
    network: Option<String>,
    #[serde(default)]
    groups: Option<Vec<String>>,
    #[serde(default)]
    regions: Option<Vec<String>>,
    #[serde(default, rename = "requireStakedHosts")]
    require_staked_hosts: Option<bool>,
    #[serde(default, rename = "maxEscrow")]
    max_escrow: Option<f64>,
}

/// Parse the loose camelCase override strings into the typed [`QueryExtras`].
/// Unrecognized enum strings are ignored (treated as "not provided") so a bad
/// value degrades to current behavior rather than 400-ing.
fn parse_extras(body: &QueryBody) -> QueryExtras {
    let prefer = body
        .prefer
        .as_deref()
        .and_then(|s| match s.trim().to_ascii_lowercase().as_str() {
            "local" => Some(PreferMode::Local),
            "remote" => Some(PreferMode::Remote),
            "auto" => Some(PreferMode::Auto),
            _ => None,
        });
    let payment = body
        .payment
        .as_deref()
        .and_then(|s| match s.trim().to_ascii_lowercase().as_str() {
            "free" => Some(PaymentPref::Free),
            "paid" => Some(PaymentPref::Paid),
            "auto" => Some(PaymentPref::Auto),
            _ => None,
        });
    let min_attestation = body.min_attestation.as_deref().and_then(|s| {
        match s.trim().to_ascii_uppercase().as_str() {
            "L0" | "L1" | "L2" => Some(s.trim().to_ascii_uppercase()),
            _ => None,
        }
    });
    let clean = |v: &Option<Vec<String>>| {
        v.as_ref().map(|xs| {
            xs.iter()
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect::<Vec<_>>()
        })
    };
    QueryExtras {
        prefer,
        payment,
        min_trust: body.min_trust,
        min_attestation,
        network: body
            .network
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        groups: clean(&body.groups),
        regions: clean(&body.regions),
        require_staked_hosts: body.require_staked_hosts,
        // Ignore non-positive caps (treat as "not provided").
        max_escrow: body.max_escrow.filter(|m| *m > 0.0),
    }
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
    // Parse the optional override set BEFORE moving owned fields out of `body`.
    let extras = parse_extras(&body);
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
        extras,
        resp: Some(tx),
    };
    if s.q_tx.send(req).await.is_err() {
        return Json(json!({"error": "grid offline"}));
    }
    Json(rx.await.unwrap_or_else(|_| json!({"error": "grid busy"})))
}

// --------------------------------------------------------------------------

// Ambient/warmup analytics. These now run on the REAL DuckDB engine on the honest
// hosts, so each query is fully SELF-CONTAINED (synthesizes its source table via a
// `range()` CTE) — realistic shape, deterministic across workers (so quorum agrees),
// no external tables/files required.
const AMBIENT_SQL: &[&str] = &[
    "WITH orders AS (SELECT i%5 AS region, (i*7)%1000 AS total FROM range(5000) t(i)) \
     SELECT region, count(*) AS orders, sum(total) AS gmv FROM orders GROUP BY region ORDER BY region",
    "WITH telemetry AS (SELECT i%24 AS hour, (i*13)%500 AS latency_ms FROM range(4800) t(i)) \
     SELECT hour, avg(latency_ms) AS avg_latency FROM telemetry GROUP BY hour ORDER BY hour",
    "WITH sales AS (SELECT i%40 AS sku, (i*3)%50 AS qty FROM range(6000) t(i)) \
     SELECT sku, sum(qty) AS qty FROM sales GROUP BY sku HAVING sum(qty) > 1000 ORDER BY sku",
    "WITH events AS (SELECT i%8 AS cohort, i%500 AS user_id FROM range(4000) t(i)) \
     SELECT cohort, count(DISTINCT user_id) AS users FROM events GROUP BY cohort ORDER BY cohort",
    "WITH invoices AS (SELECT i%20 AS country, (i*11)%1000 AS revenue FROM range(6000) t(i)) \
     SELECT country, sum(revenue) AS revenue FROM invoices GROUP BY country ORDER BY revenue DESC LIMIT 20",
    "WITH predictions AS (SELECT i%6 AS model, (i*17)%100 AS score FROM range(3000) t(i)) \
     SELECT model, avg(score) AS score FROM predictions GROUP BY model ORDER BY model",
    "WITH jobs AS (SELECT i%3 AS status FROM range(1500) t(i)) \
     SELECT status, count(*) AS n FROM jobs GROUP BY status ORDER BY status",
    "WITH payments AS (SELECT i%4 AS tier, (i*5)%500 AS amount FROM range(2500) t(i)) \
     SELECT tier, sum(amount) AS amount FROM payments GROUP BY tier ORDER BY tier",
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
    // Shared demo attestation authority (console-server DEMO only): every L1/L2
    // host's MockAttestor signs with this key, and the coordinator's
    // AllowlistVerifier below trusts exactly this authority. Generated per process.
    let demo_authority = SigningKey::generate(&mut OsRng);
    let demo_authority_pub = demo_authority.verifying_key().to_bytes();
    let mut workers = Vec::new();
    for s in &specs {
        workers.push(spawn_worker(s, &demo_authority).await);
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
    // Verify L1/L2 evidence against the shared demo authority + allowlisted
    // measurements (required_level L0 ⇒ the per-class min-attestation floor does
    // the gating; the verifier just proves the evidence is genuine + nonce-bound).
    // Without this, the honest-default fail-closed gate would downgrade every
    // demo L1/L2 host to L0 and Internal/Sensitive jobs would find no hosts.
    let attestation_verifier = Arc::new(AllowlistVerifier::new(
        demo_authority_pub,
        [
            demo_measurement(AttestationLevel::L1).to_string(),
            demo_measurement(AttestationLevel::L2).to_string(),
        ],
        AttestationLevel::L0,
    ));
    let coord = Coordinator::new(req, disc, store.clone(), cfg, "mock-1")
        .with_stake_registry(reg.clone())
        .with_settlement(Arc::new(MockSettlement::new()))
        .with_record_anchor(Arc::new(InMemoryRecordAnchor::new()))
        .with_attestation_verifier(attestation_verifier);

    let mut grid = Grid {
        workers,
        store,
        reg,
        coord,
        stakes,
        fee_pct: eco.fees.platform_fee_pct,
        commission_frac: eco.fees.participation_commission_frac,
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
            QueryExtras::default(),
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
                        reqq.extras,
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
                        extras: QueryExtras::default(),
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
