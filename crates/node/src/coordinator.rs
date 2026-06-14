//! Requester-side hedged execution scheduler (architecture §11).
//!
//! Pipeline: discover bounded candidates → Offer/Bid → select top-`k` by
//! effective trust + ETA → Dispatch to `k` → collect commit hashes → quorum →
//! stream from the fastest *agreeing* worker, RESET the losers → emit signed
//! receipts and update reputation.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use p2p_config::{DataClassCfg, GridConfig, QueryOverrides, VerifyModeCfg};
use p2p_proto::{
    Ack, AttestationLevel, Bid, BidDecision, Cancel, DataClass, Dispatch, JobId, NodeId, Offer,
    QueryHash, Receipt, ResultCommit, ResultSet, Verdict, VerifyMode, Wire,
};
use p2p_settlement::StakeRegistry;
use p2p_transport::endpoint::{read_msg, write_msg};
use p2p_transport::{Conn, NodeIdentity, QuicTransport, RecvStream, SendStream, Transport};
use p2p_trust::{
    age_factor, attestation_gate, canonical, exploration_bonus, is_nondeterministic, now_ts,
    requester_trust_weight, sign_receipt, soft_trust_score, ReceiptDraft, TrustInputs, TrustStore,
};
use rand::Rng;
use tracing::debug;

use crate::antiabuse::Blocklist;
use crate::canary::CanaryAuditor;
use crate::discovery::{CandidateFilter, Discovery};
use crate::engine::ExecLease;
use crate::estimator::WorkingSetEstimate;
use crate::planner::{is_resource_exhaustion, LocalExecutor, LocalOrRemotePlanner, PlanRequest};
use crate::signer::IdentitySigner;

/// Errors from running a query on the grid.
#[derive(Debug, thiserror::Error)]
pub enum CoordinatorError {
    #[error("config error: {0}")]
    Config(#[from] p2p_config::ConfigError),
    #[error(
        "no hosts available to run this query on the grid. Join a network with \
         p2p_join(bootstrap => [...]) or add bootstrap seeds (discovery.bootstrap), \
         and ensure reachable hosts have called p2p_share. (In remote-only mode the \
         node will not fall back to running the query locally.)"
    )]
    NoCandidates,
    #[error("not enough trustworthy workers: have {have}, need quorum {quorum}")]
    InsufficientWorkers { have: usize, quorum: usize },
    #[error("quorum not reached: best agreement {agreement}/{quorum}")]
    QuorumFailed { agreement: usize, quorum: usize },
    #[error("winner did not return a result")]
    NoResult,
    #[error("local execution failed: {0}")]
    LocalExecution(String),
    #[error("transport error: {0}")]
    Transport(#[from] p2p_transport::TransportError),
}

/// The outcome of a grid query.
#[derive(Debug, Clone)]
pub struct QueryOutcome {
    pub job_id: JobId,
    pub result: ResultSet,
    pub agreed_hash: Option<String>,
    pub agreement: usize,
    pub quorum: usize,
    pub verified: bool,
    pub winner: Option<NodeId>,
    pub participants: Vec<NodeId>,
    pub receipts: Vec<Receipt>,
    /// True when the query ran on the **free local path** (this node's own
    /// in-process locked-down DuckDB) with no bidding/escrow/quorum/payment.
    /// `false` for grid-dispatched queries.
    pub executed_locally: bool,
}

/// Requester coordinator.
#[derive(Clone)]
pub struct Coordinator {
    transport: Arc<QuicTransport>,
    discovery: Arc<dyn Discovery>,
    trust_store: Arc<dyn TrustStore>,
    base_config: Arc<GridConfig>,
    engine_version: String,
    canary: Option<Arc<CanaryAuditor>>,
    /// Optional free local-execution path (engine + headroom/slot accounting).
    /// When set alongside `planner`, queries the planner routes `Local` run here
    /// instead of being dispatched to the grid.
    local: Option<Arc<LocalExecutor>>,
    /// Optional local-vs-remote routing policy.
    planner: Option<Arc<dyn LocalOrRemotePlanner>>,
    /// Optional stake registry (economics seam, BLOCKCHAIN_ECONOMICS §5.2/§10.1).
    /// Consulted ONLY for grid jobs that resolve to the PAID mode while
    /// `economics.enabled`; otherwise the worker's `stake_factor` stays `0.0`
    /// (today's behavior). Free jobs never touch it. This is the single
    /// economics-gated input to the trust score — reputation/quality scoring is
    /// independent and always runs (see `trust_store.record` below).
    stake_registry: Option<Arc<dyn StakeRegistry>>,
    /// Optional local deny-list (ARCHITECTURE "Abuse resistance"). When set,
    /// blocked candidates are excluded from selection and auto-block triggers
    /// add to it. `None` (default) ⇒ no blocking, exactly today's behavior.
    blocklist: Option<Arc<Blocklist>>,
}

/// One worker that committed a result hash and whose decision stream is open.
struct InFlight {
    worker: NodeId,
    send: SendStream,
    recv: RecvStream,
    hash: String,
    latency_ms: u64,
}

impl Coordinator {
    pub fn new(
        transport: Arc<QuicTransport>,
        discovery: Arc<dyn Discovery>,
        trust_store: Arc<dyn TrustStore>,
        base_config: Arc<GridConfig>,
        engine_version: impl Into<String>,
    ) -> Self {
        Self {
            transport,
            discovery,
            trust_store,
            base_config,
            engine_version: engine_version.into(),
            canary: None,
            local: None,
            planner: None,
            stake_registry: None,
            blocklist: None,
        }
    }

    /// Wire a local deny-list (ARCHITECTURE "Abuse resistance"): blocked
    /// candidates are excluded from selection, and the auto-block trigger
    /// (`[antiabuse.blocklist].auto_block_*`) adds to it. Off by default.
    pub fn with_blocklist(mut self, blocklist: Arc<Blocklist>) -> Self {
        self.blocklist = Some(blocklist);
        self
    }

    /// Wire a stake registry into the economics `stake_factor` seam. Off by
    /// default; even when set it only affects PAID grid jobs while
    /// `economics.enabled` (free jobs and the disabled chain are unaffected).
    pub fn with_stake_registry(mut self, registry: Arc<dyn StakeRegistry>) -> Self {
        self.stake_registry = Some(registry);
        self
    }

    pub fn with_canary(mut self, auditor: Arc<CanaryAuditor>) -> Self {
        self.canary = Some(auditor);
        self
    }

    /// Enable the free local-execution path: queries the `planner` routes
    /// `Local` run on `local`'s in-process locked-down engine with NO
    /// bidding/escrow/quorum/payment. Without this, every query is dispatched to
    /// the grid exactly as before.
    pub fn with_local_execution(
        mut self,
        local: Arc<LocalExecutor>,
        planner: Arc<dyn LocalOrRemotePlanner>,
    ) -> Self {
        self.local = Some(local);
        self.planner = Some(planner);
        self
    }

    fn identity(&self) -> &NodeIdentity {
        self.transport.identity()
    }

    /// The shared QUIC transport backing this coordinator (used to also stand up
    /// a co-located [`crate::worker::Worker`] on the same endpoint/identity).
    pub fn transport(&self) -> Arc<QuicTransport> {
        Arc::clone(&self.transport)
    }

    /// This node's identity as a requester (used to key its requester reputation
    /// / age for the trust-weighting mechanism). Exposed for tests/tooling.
    pub fn local_node_id(&self) -> &NodeId {
        self.transport.local_node_id()
    }

    /// Run a query with optional per-call overrides.
    ///
    /// First consults the local-vs-remote planner (if local execution is
    /// configured). When the planner chooses the local path the query runs for
    /// free in this node's own engine and returns immediately; otherwise it is
    /// dispatched to the grid (hedged/quorum) exactly as before. Without a
    /// pre-flight estimate the integrated decision covers forced-local /
    /// forced-remote / saturation; estimate-driven `auto` routing is available
    /// via [`Coordinator::run_query_planned`].
    pub async fn run_query(
        &self,
        sql: &str,
        overrides: QueryOverrides,
    ) -> Result<QueryOutcome, CoordinatorError> {
        let cfg = overrides.apply(&self.base_config)?;

        // Local-first hook (minimal): the routing decision itself lives in the
        // `planner` module; this just acts on it.
        if let Some(outcome) = self.try_local_execution(sql, &cfg, None).await? {
            return Ok(outcome);
        }

        let data_class = DataClass::Public; // class selection is a future extension
        // Resolve the per-job payment mode (free vs paid). Free jobs run the exact
        // off-chain path (no escrow/stake/anchor/fees); only PAID jobs feed the
        // economics `stake_factor` into selection. Scoring runs regardless.
        let paid = cfg
            .economics
            .resolve_payment(data_class_to_cfg(data_class))
            .is_paid();
        let min_level = parse_level(&cfg.trust.min_attestation);
        let job_id = JobId::new();
        let query_hash = QueryHash::compute(sql, &self.engine_version);

        // 1. Discover a bounded candidate set.
        let filter = CandidateFilter {
            data_class,
            min_attestation: min_level,
        };
        let mut candidates = self
            .discovery
            .find_candidates(cfg.discovery.candidate_sample_size, filter)
            .await;
        // Anti-abuse: exclude blocklisted candidates (ARCHITECTURE "Abuse
        // resistance"). Each node independently refuses flagged actors.
        if cfg.antiabuse.enabled {
            if let Some(bl) = &self.blocklist {
                candidates.retain(|c| match &c.node_id {
                    Some(id) => !bl.is_blocked(id.as_str()),
                    None => true,
                });
            }
        }
        if candidates.is_empty() {
            return Err(CoordinatorError::NoCandidates);
        }

        // 2. Offer/Bid against each candidate (bounded, concurrent, timed).
        let mut conns: HashMap<NodeId, Conn> = HashMap::new();
        let mut accepted: Vec<(NodeId, Bid)> = Vec::new();
        let offer_timeout = Duration::from_millis(cfg.scheduler.offer_timeout_ms);

        let offer = Offer {
            job_id: job_id.clone(),
            requester_id: self.transport.local_node_id().clone(),
            query_hash: query_hash.clone(),
            cost_hint_rows: None,
            data_class,
            nonce: rand::thread_rng().gen(),
        };

        let offer_futures = candidates.into_iter().map(|cand| {
            let transport = Arc::clone(&self.transport);
            let offer = offer.clone();
            async move {
                let conn = tokio::time::timeout(
                    offer_timeout,
                    transport.connect(cand.addr, cand.node_id.clone()),
                )
                .await
                .ok()?
                .ok()?;
                let reply =
                    tokio::time::timeout(offer_timeout, send_offer(&conn, &offer)).await.ok()?.ok()?;
                Some((conn, reply))
            }
        });

        for res in futures_util::future::join_all(offer_futures).await {
            if let Some((conn, bid)) = res {
                let worker = conn.peer_node_id().clone();
                if let BidDecision::Accept = bid.decision {
                    conns.insert(worker.clone(), conn);
                    accepted.push((worker, bid));
                }
            }
        }

        // 3. Filter by attestation gate + min trust, score, select top-k.
        let now = now_ts();
        let ab = &cfg.antiabuse;
        let auto_block = ab.enabled
            && ab.blocklist.auto_block_enabled
            && ab.blocklist.auto_block_trust_floor > 0.0;
        let mut scored: Vec<(NodeId, f64, u64)> = accepted
            .into_iter()
            .filter(|(_, bid)| attestation_gate(bid.attestation.level, min_level))
            .map(|(worker, bid)| {
                let score = self.effective_trust(&worker, &cfg, now, paid);
                // Auto-block a worker whose effective trust is below the floor
                // (ARCHITECTURE "Abuse resistance"). It is then excluded here and
                // for all future jobs.
                if auto_block && score < ab.blocklist.auto_block_trust_floor {
                    if let Some(bl) = &self.blocklist {
                        bl.block(
                            worker.as_str(),
                            p2p_config::BlockKind::NodeId,
                            "auto-block: trust below floor",
                            "auto",
                        );
                    }
                }
                (worker, score, bid.eta_ms)
            })
            .filter(|(_, score, _)| *score >= cfg.trust.min_trust)
            .collect();

        // sort by score desc, then ETA asc
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.2.cmp(&b.2))
        });
        scored.truncate(cfg.scheduler.replicas);

        if scored.len() < cfg.scheduler.quorum {
            return Err(CoordinatorError::InsufficientWorkers {
                have: scored.len(),
                quorum: cfg.scheduler.quorum,
            });
        }

        // 4. Dispatch to selected workers and collect commits.
        let verify_mode = match cfg.scheduler.verify_mode {
            VerifyModeCfg::Fast => VerifyMode::Fast,
            VerifyModeCfg::Quorum => VerifyMode::Quorum,
        };
        let dispatch = Dispatch {
            job_id: job_id.clone(),
            sql: sql.to_string(),
            query_hash: query_hash.clone(),
            credential: None,
            memory_limit_bytes: cfg.budget.per_job_memory_bytes,
            threads: cfg.budget.per_job_threads,
            verify_mode,
            sealed_key: None,
            result_parallelism: Some(cfg.transport.result.parallelism as u32),
            compression: Some(crate::compression::algo_to_wire(cfg.transport.compression.algorithm)),
        };
        let dispatch_timeout = Duration::from_millis(cfg.scheduler.dispatch_timeout_ms);

        let mut inflight: Vec<InFlight> = Vec::new();
        let dispatch_futs = scored.iter().map(|(worker, _, _)| {
            let conn = conns.get(worker).expect("selected worker has a conn");
            let dispatch = dispatch.clone();
            let worker = worker.clone();
            async move {
                match tokio::time::timeout(dispatch_timeout, dispatch_and_commit(conn, &dispatch))
                    .await
                {
                    Ok(Ok((send, recv, commit))) => Some(InFlight {
                        worker,
                        send,
                        recv,
                        hash: commit.result_hash,
                        latency_ms: commit.latency_ms,
                    }),
                    _ => None,
                }
            }
        });
        for r in futures_util::future::join_all(dispatch_futs).await {
            if let Some(f) = r {
                inflight.push(f);
            }
        }

        // 5. Quorum decision (or canary judgement).
        let hashes: Vec<&str> = inflight.iter().map(|f| f.hash.as_str()).collect();
        let outcome = canonical::evaluate_quorum(hashes, cfg.scheduler.quorum);

        // If this is a canary, the authoritative answer is the known one.
        let canary_expected = self
            .canary
            .as_ref()
            .and_then(|c| c.expected(&query_hash));

        // Non-determinism handling (ARCHITECTURE "Abuse resistance"): a query
        // that can't produce a stable canonical hash (random()/now()/unordered
        // LIMIT/…) cannot reach a meaningful quorum. Mark it NON-VERIFIABLE,
        // return the fastest result anyway, and apply NO provider penalty for the
        // (expected) hash divergence. A canary always has an authoritative answer
        // so it is never treated as non-verifiable.
        let non_verifiable =
            cfg.antiabuse.nondeterminism_active() && canary_expected.is_none() && is_nondeterministic(sql);

        let (agreed_hash, verified) = match (&canary_expected, verify_mode) {
            (Some(expected), _) => (Some(expected.clone()), true),
            _ if non_verifiable => {
                // Fastest result wins; flagged non-verified.
                let fastest = inflight.iter().min_by_key(|f| f.latency_ms).map(|f| f.hash.clone());
                (fastest, false)
            }
            (None, VerifyMode::Quorum) => {
                if !outcome.reached() {
                    // No quorum: with fault attribution active this is attributed
                    // to the JOB (most/all providers disagreed), not the
                    // providers — so the failure receipts are NEUTRAL.
                    self.emit_failure_receipts(&inflight, &job_id, &query_hash, &cfg);
                    return Err(CoordinatorError::QuorumFailed {
                        agreement: outcome.agreement,
                        quorum: outcome.quorum,
                    });
                }
                (outcome.agreed_hash.clone(), true)
            }
            (None, VerifyMode::Fast) => {
                // Fastest result wins immediately; verification is best-effort.
                let fastest = inflight.iter().min_by_key(|f| f.latency_ms).map(|f| f.hash.clone());
                (fastest, outcome.reached())
            }
        };

        let agreed = match agreed_hash {
            Some(h) => h,
            None => return Err(CoordinatorError::NoResult),
        };

        // 6. Pick the fastest worker whose hash matches the agreed hash.
        let winner_idx = inflight
            .iter()
            .enumerate()
            .filter(|(_, f)| f.hash == agreed)
            .min_by_key(|(_, f)| f.latency_ms)
            .map(|(i, _)| i);

        let winner_idx = match winner_idx {
            Some(i) => i,
            None => {
                self.emit_failure_receipts(&inflight, &job_id, &query_hash, &cfg);
                return Err(CoordinatorError::NoResult);
            }
        };

        // Requester-trust weight (ARCHITECTURE "Abuse resistance"): a job's effect
        // on a provider's score is scaled by w(requester) ∈ [0,1] from THIS node's
        // own reputation+age as a requester. New/unproven requester ⇒ w≈0 for
        // negative outcomes, so a brand-new actor's heavy/incorrect job barely
        // moves a provider's score. When the mechanism is off, w = 1.0 (today).
        let self_id = self.transport.local_node_id().clone();
        let negative_weight = if cfg.antiabuse.requester_trust_active() {
            let obs = self.trust_store.requester_observation_count(&self_id);
            let rep = self.trust_store.requester_reputation(&self_id, now);
            requester_trust_weight(
                rep,
                obs,
                cfg.antiabuse.requester_trust.age_saturation,
                cfg.antiabuse.requester_trust.negative_floor_weight,
            )
        } else {
            1.0
        };

        // 7. Tell winner to proceed; cancel losers (RESET). Collect the result.
        let participants: Vec<NodeId> = inflight.iter().map(|f| f.worker.clone()).collect();
        let mut result: Option<ResultSet> = None;
        let mut receipts = Vec::new();
        let winner_id = inflight[winner_idx].worker.clone();

        for (i, mut f) in inflight.into_iter().enumerate() {
            // Verdict relative to the authoritative hash, with fault attribution:
            // a non-verifiable job is NEUTRAL (Inconclusive — no penalty); a hash
            // mismatch against a verified quorum is provable provider fault.
            let verdict = if non_verifiable {
                Verdict::Inconclusive
            } else if f.hash == agreed {
                Verdict::Correct
            } else {
                Verdict::Incorrect
            };

            if i == winner_idx {
                // proceed
                let _ = write_msg(
                    &mut f.send,
                    &Wire::Ack(Ack {
                        job_id: job_id.clone(),
                        ok: true,
                        detail: "proceed".into(),
                    }),
                )
                .await;
                if let Some(conn) = conns.get(&f.worker) {
                    if let Ok(rs) = crate::result_stream::recv_result(conn, &mut f.recv).await {
                        result = Some(rs);
                    }
                }
                let _ = f.send.finish();
            } else {
                // RESET loser
                let _ = write_msg(
                    &mut f.send,
                    &Wire::Cancel(Cancel {
                        job_id: job_id.clone(),
                        reason: "lost race".into(),
                    }),
                )
                .await;
                let _ = f.send.finish();
            }

            receipts.push(self.make_receipt(&job_id, &f.worker, &query_hash, &f.hash, verdict, f.latency_ms));
            // Penalize ONLY provable provider fault, scaled by the requester's
            // standing. A new/unproven requester (negative_weight ≈ 0) can barely
            // move a provider's score — the griefing defense. The penalty floors
            // at 0 (never goes negative).
            if verdict.is_provider_fault() {
                let penalty = (self.base_config.trust.incorrect_penalty * negative_weight).max(0.0);
                if penalty > 0.0 {
                    self.trust_store.penalize(&f.worker, penalty);
                }
            }
        }

        // record all receipts in the trust store (neutral verdicts are skipped
        // inside `record`, so a non-verifiable / job-caused outcome leaves
        // provider reputation untouched).
        for r in &receipts {
            self.trust_store.record(r);
        }

        // The requester accrues its own age/reputation from completed jobs, which
        // feeds the requester-trust weighting above on future jobs.
        self.trust_store.record_requester(&self_id, verified, now);

        let result = result.ok_or(CoordinatorError::NoResult)?;

        Ok(QueryOutcome {
            job_id,
            result,
            agreed_hash: Some(agreed),
            agreement: outcome.agreement,
            quorum: outcome.quorum,
            verified,
            winner: Some(winner_id),
            participants,
            receipts,
            executed_locally: false,
        })
    }

    /// Like [`Coordinator::run_query`] but with a pre-flight working-set
    /// `estimate` so the planner's `auto` mode can route by estimated peak RAM
    /// vs. current local headroom. Falls back to the grid when the planner
    /// chooses remote (or local execution is not configured / fails over).
    pub async fn run_query_planned(
        &self,
        sql: &str,
        overrides: QueryOverrides,
        estimate: Option<WorkingSetEstimate>,
    ) -> Result<QueryOutcome, CoordinatorError> {
        let cfg = overrides.apply(&self.base_config)?;
        if let Some(outcome) = self.try_local_execution(sql, &cfg, estimate).await? {
            return Ok(outcome);
        }
        // Planner chose remote (or failed over): dispatch to the grid. `run_query`
        // re-evaluates the (estimate-less) hook, which is a no-op here since the
        // decision was already remote.
        self.run_query(sql, overrides).await
    }

    /// Consult the planner and, if it chooses the local path, execute the query
    /// for free on the in-process engine. Returns `Ok(None)` to mean "go to the
    /// grid" (planner chose remote, local execution not configured, locally
    /// saturated, or an adaptive fail-over after a mid-flight resource blow-up).
    async fn try_local_execution(
        &self,
        sql: &str,
        cfg: &GridConfig,
        estimate: Option<WorkingSetEstimate>,
    ) -> Result<Option<QueryOutcome>, CoordinatorError> {
        let (local, planner) = match (&self.local, &self.planner) {
            (Some(l), Some(p)) => (l, p),
            _ => return Ok(None), // local execution not configured → grid
        };

        let prefer = cfg.planner.prefer;
        let decision = planner.decide(&PlanRequest {
            prefer,
            estimate: estimate.clone(),
            headroom_bytes: local.headroom_bytes(),
            local_slot_available: local.slot_available(),
        });
        debug!("planner decision: {decision:?}");
        if !decision.is_local() {
            return Ok(None);
        }

        // Reserve a local slot + headroom (the estimate's peak, if known). A
        // failed reservation means we lost the slot race → grid.
        let reserve_bytes = estimate
            .as_ref()
            .map(|e| e.peak_working_set_bytes)
            .unwrap_or(0);
        let reservation = match local.reserve(reserve_bytes) {
            Some(r) => r,
            None => return Ok(None),
        };

        // Free local path: no Offer/Bid/Dispatch, no quorum, no receipts, no
        // payment. Give DuckDB a real memory_limit so a working set that blows
        // past the local budget errors → adaptive fail-over below.
        let lease = ExecLease {
            memory_bytes: local
                .local_budget_bytes()
                .saturating_add(cfg.planner.spill_tolerance_bytes)
                .max(64 * 1024 * 1024),
            threads: cfg.budget.per_job_threads.max(1),
        };
        let job_id = JobId::new();
        let exec = local.engine().execute(sql, lease).await;
        drop(reservation); // release headroom + slot regardless of outcome

        match exec {
            Ok(result) => {
                let self_id = self.transport.local_node_id().clone();
                Ok(Some(QueryOutcome {
                    job_id,
                    result,
                    agreed_hash: None,
                    agreement: 1,
                    quorum: 0,
                    verified: true, // own machine — trusted by definition
                    winner: Some(self_id.clone()),
                    participants: vec![self_id],
                    receipts: Vec::new(),
                    executed_locally: true,
                }))
            }
            Err(e) => {
                // Adaptive fail-over: a mid-flight resource blow-up aborts local
                // and re-dispatches to the grid (unless the caller pinned local).
                if is_resource_exhaustion(&e) && planner.failover_to_remote(prefer) {
                    debug!("local execution exhausted resources; failing over to grid: {e}");
                    return Ok(None);
                }
                Err(CoordinatorError::LocalExecution(e.to_string()))
            }
        }
    }

    fn effective_trust(&self, worker: &NodeId, cfg: &GridConfig, now: u64, paid: bool) -> f64 {
        // Economics seam (§5.2/§10.1): the diminishing/capped stake factor feeds
        // the trust score ONLY for paid jobs while economics is enabled and a
        // stake registry is wired. Otherwise it stays 0.0 — exactly today's
        // behavior — so a free job (or a chain-off node) is scored identically
        // minus any stake nudge.
        let stake_factor = if paid && cfg.economics.enabled {
            self.stake_registry
                .as_ref()
                .map(|r| r.stake_factor(worker))
                .unwrap_or(0.0)
        } else {
            0.0
        };
        // Confidence-aware reputation (BLOCKCHAIN_ECONOMICS §4.1/§7.3): the raw
        // success ratio is replaced by a Beta/Wilson lower-confidence bound so a
        // node with thin history is not treated as fully trusted. Unknown nodes
        // (no history) still fall back to the bootstrap value, exactly as before.
        let rep = &cfg.economics.reputation;
        let reputation = self
            .trust_store
            .confident_reputation(worker, now, rep.prior_alpha, rep.prior_beta, rep.confidence_z)
            .unwrap_or(cfg.trust.bootstrap_trust);
        let obs = self.trust_store.observation_count(worker);
        let inputs = TrustInputs {
            reputation,
            age_factor: age_factor(obs, 20),
            voucher_trust: self.trust_store.voucher_trust(worker).min(1.0),
            stake_factor,
            penalties: self.trust_store.penalty(worker),
        };
        // Cold-start exploration (§5.2/§6): add a decaying uncertainty bonus so
        // new honest nodes get sampled and can build reputation. ε defaults to
        // 0.0 (pure exploitation — today's behavior) and is configurable.
        let bonus = exploration_bonus(
            obs,
            cfg.economics.ranking.exploration_rate,
            cfg.economics.ranking.exploration_saturation,
        );
        (soft_trust_score(&cfg.trust.weights, &inputs) + bonus).clamp(0.0, 1.0)
    }

    fn make_receipt(
        &self,
        job_id: &JobId,
        worker: &NodeId,
        query_hash: &QueryHash,
        result_hash: &str,
        verdict: Verdict,
        latency_ms: u64,
    ) -> Receipt {
        let draft = ReceiptDraft {
            job_id: job_id.clone(),
            worker_id: worker.clone(),
            query_hash: query_hash.clone(),
            result_hash: result_hash.to_string(),
            verdict,
            latency_ms,
            ts: now_ts(),
        };
        sign_receipt(draft, &IdentitySigner(self.identity()))
    }

    fn emit_failure_receipts(
        &self,
        inflight: &[InFlight],
        job_id: &JobId,
        query_hash: &QueryHash,
        cfg: &GridConfig,
    ) {
        // A no-quorum failure means the selected providers did NOT agree. With
        // fault attribution active we treat this as job/non-attributable
        // (Inconclusive — neutral, no provider penalty) rather than blaming every
        // provider, since a genuine minority cheater is caught on the success
        // path. With it off, fall back to the historical Malformed verdict.
        let verdict = if cfg.antiabuse.fault_attribution_active() {
            Verdict::Inconclusive
        } else {
            Verdict::Malformed
        };
        for f in inflight {
            let r = self.make_receipt(job_id, &f.worker, query_hash, &f.hash, verdict, f.latency_ms);
            self.trust_store.record(&r);
        }
        debug!("emitted failure receipts for job {job_id}");
    }
}

fn parse_level(s: &str) -> AttestationLevel {
    match s {
        "L1" => AttestationLevel::L1,
        "L2" => AttestationLevel::L2,
        _ => AttestationLevel::L0,
    }
}

/// Map the proto data class onto the config mirror used by the economics layer.
fn data_class_to_cfg(c: DataClass) -> DataClassCfg {
    match c {
        DataClass::Public => DataClassCfg::Public,
        DataClass::Internal => DataClassCfg::Internal,
        DataClass::Sensitive => DataClassCfg::Sensitive,
    }
}

/// Send an Offer and read the Bid on a fresh stream.
async fn send_offer(conn: &Conn, offer: &Offer) -> Result<Bid, p2p_transport::TransportError> {
    let (mut send, mut recv) = conn.open_bi().await?;
    write_msg(&mut send, &Wire::Offer(offer.clone())).await?;
    let _ = send.finish();
    match read_msg(&mut recv).await? {
        Wire::Bid(b) => Ok(b),
        other => Err(p2p_transport::TransportError::Connection(format!(
            "expected Bid, got {other:?}"
        ))),
    }
}

/// Open a dispatch stream, send the Dispatch, read the commit. Leaves the stream
/// open so the requester can later proceed/cancel.
async fn dispatch_and_commit(
    conn: &Conn,
    dispatch: &Dispatch,
) -> Result<(SendStream, RecvStream, ResultCommit), p2p_transport::TransportError> {
    let (mut send, mut recv) = conn.open_bi().await?;
    write_msg(&mut send, &Wire::Dispatch(dispatch.clone())).await?;
    match read_msg(&mut recv).await? {
        Wire::Commit(c) => Ok((send, recv, c)),
        Wire::Ack(a) => Err(p2p_transport::TransportError::Connection(format!(
            "worker declined: {}",
            a.detail
        ))),
        other => Err(p2p_transport::TransportError::Connection(format!(
            "expected Commit, got {other:?}"
        ))),
    }
}