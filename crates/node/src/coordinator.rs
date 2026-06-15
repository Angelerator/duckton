//! Requester-side hedged execution scheduler (architecture §11).
//!
//! Pipeline: discover bounded candidates → Offer/Bid → select top-`k` by
//! effective trust + ETA → Dispatch to `k` → collect commit hashes → quorum →
//! stream from the fastest *agreeing* worker, RESET the losers → emit signed
//! receipts and update reputation.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use p2p_config::{DataClassCfg, GridConfig, QueryOverrides, VerifyModeCfg};
use p2p_proto::{
    Ack, AttestationLevel, Bid, BidDecision, Cancel, DataClass, Dispatch, JobId, NodeId, Offer,
    Progress, QueryHash, Receipt, ResultSet, Verdict, VerifyMode, Wire,
};
use p2p_settlement::{
    Amount, JobRecord, OnchainPolicy, ParamsSource, Payout, RecordAnchor, Settlement,
    SettlementOutcome, SlashReason, StakeRegistry, WalletAddress,
};
use p2p_transport::endpoint::{read_msg, write_msg};
use p2p_transport::{Conn, NodeIdentity, QuicTransport, RecvStream, SendStream, Transport};
use p2p_trust::{
    age_factor, attestation_gate, canonical, classify_failure, exploration_bonus,
    is_nondeterministic, now_ts, requester_trust_weight, sign_receipt, soft_trust_score,
    ReceiptDraft, TrustInputs, TrustStore,
};
use rand::Rng;
use tracing::debug;

use crate::antiabuse::Blocklist;
use crate::canary::CanaryAuditor;
use crate::discovery::{CandidateFilter, Discovery};
use crate::engine::ExecLease;
use crate::estimator::WorkingSetEstimate;
use crate::liveness::{now_ms, LivenessView};
use crate::planner::{is_resource_exhaustion, LocalExecutor, LocalOrRemotePlanner, PlanRequest};
use crate::retry::{Backoff, FaultTally, TokenBucket};
use crate::signer::IdentitySigner;

/// QUIC application error code used when the coordinator abruptly RESETs a
/// loser's (or a stalled winner's) dispatch stream. The worker maps a reset on
/// the dispatch stream to a prompt job abort (it does not interpret the code).
const LOSER_RESET_CODE: quinn::VarInt = quinn::VarInt::from_u32(7);

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
    #[error(
        "query is infeasible — a consensus of selected providers failed it the same way \
         (job fault, not a provider fault): {reason}"
    )]
    Infeasible { reason: String },
    #[error(
        "re-dispatch exhausted after {attempts} attempt(s) without a usable result \
         (last reason: {reason})"
    )]
    Exhausted { attempts: u32, reason: String },
    #[error("retry/hedge budget exhausted after {attempts} attempt(s) — stopping to avoid a retry storm")]
    RetryBudgetExhausted { attempts: u32 },
    #[error("winner did not return a result")]
    NoResult,
    #[error("settlement error: {0}")]
    Settlement(String),
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

/// Latest streamed [`Progress`] per in-flight job, surfaced so a future SQL /
/// status view can read "what is this job doing right now". Bounded by the
/// number of concurrent jobs; an entry is overwritten by each newer heartbeat
/// and cleared when the job ends.
#[derive(Default)]
pub struct ProgressTracker {
    inner: Mutex<HashMap<JobId, Progress>>,
}

impl ProgressTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the latest progress for a job (keeps only the newest by `seq`).
    pub fn update(&self, p: Progress) {
        let mut g = self.inner.lock().unwrap();
        match g.get(&p.job_id) {
            Some(prev) if prev.seq > p.seq => {}
            _ => {
                g.insert(p.job_id.clone(), p);
            }
        }
    }

    /// The latest progress for `job`, if any has been observed.
    pub fn latest(&self, job: &JobId) -> Option<Progress> {
        self.inner.lock().unwrap().get(job).cloned()
    }

    /// Snapshot of all currently-tracked job progress (for a status surface).
    pub fn snapshot(&self) -> Vec<Progress> {
        self.inner.lock().unwrap().values().cloned().collect()
    }

    /// Drop tracking for a finished job.
    pub fn clear(&self, job: &JobId) {
        self.inner.lock().unwrap().remove(job);
    }
}

/// The outcome of one (re)dispatch attempt within the resilient loop.
enum AttemptResult {
    /// A usable result — return it.
    Done(Box<QueryOutcome>),
    /// Dispatched providers did not deliver enough commits (silence / timeout /
    /// stall / mixed transient failures): re-dispatch to a fresh set. `tried` are
    /// the providers contacted this attempt (excluded next time).
    Inconclusive { tried: Vec<NodeId> },
    /// A consensus of selected providers failed the **same deterministic way**
    /// (the query is infeasible) — STOP (job fault, no provider penalty).
    Infeasible { reason: String },
    /// Enough providers committed but their result hashes did not agree — a
    /// genuine verification disagreement (terminal).
    QuorumDisagreement { agreement: usize, quorum: usize },
    /// No candidates were available to dispatch to this attempt.
    NoCandidates,
    /// Too few providers cleared selection (trust/attestation gate).
    InsufficientWorkers { have: usize, quorum: usize },
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
    /// Optional money rail (BLOCKCHAIN_ECONOMICS §8/§10.1). Consulted ONLY for
    /// grid jobs that resolve to PAID while `economics.enabled`: the requester's
    /// max bid is escrowed before dispatch and released per the quorum verdict.
    /// `None` (default) and FREE jobs never touch it — zero chain interaction.
    settlement: Option<Arc<dyn Settlement>>,
    /// Optional tamper-proof record anchor (§7): a settled paid job's `JobRecord`
    /// is appended to the off-chain epoch tree (root anchored on-chain elsewhere).
    record_anchor: Option<Arc<dyn RecordAnchor>>,
    /// Resolves a worker `NodeId` to its bound payout wallet. Defaults to a
    /// deterministic derivation; the live wiring injects the real node↔wallet
    /// binding lookup.
    wallet_resolver: Option<Arc<dyn Fn(&NodeId) -> WalletAddress + Send + Sync>>,
    /// Optional local deny-list (ARCHITECTURE "Abuse resistance"). When set,
    /// blocked candidates are excluded from selection and auto-block triggers
    /// add to it. `None` (default) ⇒ no blocking, exactly today's behavior.
    blocklist: Option<Arc<Blocklist>>,
    /// Optional liveness view (phi-accrual + SWIM, architecture §8). When set,
    /// candidates the detector has convicted (and SWIM did not rescue) are
    /// excluded from selection. `None` (default) ⇒ no liveness filtering.
    liveness: Option<Arc<LivenessView>>,
    /// Latest streamed progress per job (resilience §11) — exposed for a status
    /// surface and used to reset the stall timer during commit collection.
    progress: Arc<ProgressTracker>,
    /// On-chain `GlobalParams` policy + version/seqno in force (BLOCKCHAIN_ECONOMICS
    /// §12), refreshed at startup + on a periodic interval by the sync task. The
    /// cached `version` is bound into each settled paid job's anchored record and
    /// per-job escrow terms; the cached `policy` (when present) is overlaid onto a
    /// paid job's live `EconomicsConfig`/`SchedulerConfig`. `version == 0` +
    /// `policy == None` (default) = unbound. Shared (`Arc`) so the background sync
    /// task and the query path see the same cell.
    synced_params: Arc<SyncedParams>,
    /// Optional read seam for the on-chain `GlobalParams` policy. `None` (default)
    /// ⇒ no chain reads at all (free/local nodes). When wired, the coordinator
    /// syncs it at startup + periodically via [`Coordinator::spawn_params_sync`].
    params_source: Option<Arc<dyn ParamsSource>>,
}

/// Cached on-chain `GlobalParams` policy, shared between the periodic sync task
/// and the query path. Defaults to unbound (`version = 0`, no policy overlay).
#[derive(Default)]
struct SyncedParams {
    state: Mutex<SyncedParamsState>,
}

#[derive(Clone, Copy, Default)]
struct SyncedParamsState {
    version: u32,
    policy: Option<OnchainPolicy>,
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
            settlement: None,
            record_anchor: None,
            wallet_resolver: None,
            blocklist: None,
            liveness: None,
            progress: Arc::new(ProgressTracker::new()),
            synced_params: Arc::new(SyncedParams::default()),
            params_source: None,
        }
    }

    /// Bind the on-chain `GlobalParams` version/seqno (BLOCKCHAIN_ECONOMICS §12)
    /// that paid jobs run under. Read from the chain via
    /// [`p2p_settlement::GlobalParamsClient`] (startup or periodic sync) and
    /// stamped into every settled paid job's anchored record, so settlement and
    /// dispute resolution reference the exact params in force. `0` (default) =
    /// unbound; free jobs never anchor a record regardless.
    pub fn with_params_version(self, version: u32) -> Self {
        self.synced_params.state.lock().unwrap().version = version;
        self
    }

    /// Wire the on-chain `GlobalParams` read seam (a
    /// [`p2p_settlement::GlobalParamsClient`] over any RPC). Off by default; when
    /// set, [`Coordinator::spawn_params_sync`] / [`Coordinator::sync_params_once`]
    /// refresh the cached policy + version. Free/local jobs never consult it.
    pub fn with_params_source(mut self, source: Arc<dyn ParamsSource>) -> Self {
        self.params_source = Some(source);
        self
    }

    /// The currently-cached on-chain `GlobalParams` version/seqno (`0` = unbound).
    /// Stamped into each settled paid job's record + per-job escrow terms.
    pub fn current_params_version(&self) -> u32 {
        self.synced_params.state.lock().unwrap().version
    }

    /// The currently-cached on-chain policy (overlaid onto a paid job's live
    /// config), or `None` when nothing has been synced.
    fn synced_policy(&self) -> Option<OnchainPolicy> {
        self.synced_params.state.lock().unwrap().policy
    }

    /// Read the on-chain `GlobalParams` policy ONCE through the wired source and
    /// cache it (version + policy). This is the startup sync; it is also the unit
    /// the periodic task repeats. Errors if no source is wired or the read fails
    /// — callers on the free/local path never invoke it. NOTE: this performs a
    /// blocking RPC read; the periodic task runs it on a blocking thread.
    pub fn sync_params_once(&self) -> Result<OnchainPolicy, CoordinatorError> {
        let src = self
            .params_source
            .as_ref()
            .ok_or_else(|| CoordinatorError::Settlement("no GlobalParams source wired".into()))?;
        let policy = src
            .read_policy()
            .map_err(|e| CoordinatorError::Settlement(e.to_string()))?;
        let mut g = self.synced_params.state.lock().unwrap();
        g.version = policy.version;
        g.policy = Some(policy);
        Ok(policy)
    }

    /// Spawn the startup + periodic `GlobalParams` sync: read the on-chain policy
    /// immediately, then re-read every `interval`, overlaying it onto paid jobs'
    /// live config and caching the current `params_version`. A no-op task (logs +
    /// returns) when no source is wired, so free/local nodes are unaffected. The
    /// blocking RPC read runs on a blocking thread so the async runtime is not
    /// stalled. Returns the task handle (drop/abort to stop).
    pub fn spawn_params_sync(&self, interval: Duration) -> tokio::task::JoinHandle<()> {
        let coord = self.clone();
        tokio::spawn(async move {
            if coord.params_source.is_none() {
                debug!("GlobalParams sync not started: no source wired");
                return;
            }
            loop {
                let c = coord.clone();
                match tokio::task::spawn_blocking(move || c.sync_params_once()).await {
                    Ok(Ok(p)) => debug!("GlobalParams synced: version={}", p.version),
                    Ok(Err(e)) => debug!("GlobalParams sync failed: {e}"),
                    Err(e) => debug!("GlobalParams sync task join error: {e}"),
                }
                if interval.is_zero() {
                    break;
                }
                tokio::time::sleep(interval).await;
            }
        })
    }

    /// Wire a liveness view (phi-accrual + SWIM, architecture §8): convicted
    /// peers the detector marks dead (and SWIM does not rescue) are excluded
    /// from candidate selection. Off by default (no liveness filtering).
    pub fn with_liveness(mut self, view: Arc<LivenessView>) -> Self {
        self.liveness = Some(view);
        self
    }

    /// The shared progress tracker (latest streamed heartbeat per job), for a
    /// SQL/status surface.
    pub fn progress_tracker(&self) -> Arc<ProgressTracker> {
        Arc::clone(&self.progress)
    }

    /// Wire the money rail (BLOCKCHAIN_ECONOMICS §8/§10.1). Off by default; even
    /// when set it only engages for PAID grid jobs while `economics.enabled`
    /// (free jobs and chain-off nodes never touch it). Use the deterministic
    /// `mock` impl in tests and the `ton` impl in production.
    pub fn with_settlement(mut self, settlement: Arc<dyn Settlement>) -> Self {
        self.settlement = Some(settlement);
        self
    }

    /// Wire the record anchor (§7): a settled paid job's record is appended here.
    pub fn with_record_anchor(mut self, anchor: Arc<dyn RecordAnchor>) -> Self {
        self.record_anchor = Some(anchor);
        self
    }

    /// Wire a `NodeId → payout wallet` resolver (the node↔wallet binding lookup).
    pub fn with_wallet_resolver(
        mut self,
        resolver: Arc<dyn Fn(&NodeId) -> WalletAddress + Send + Sync>,
    ) -> Self {
        self.wallet_resolver = Some(resolver);
        self
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
        let mut cfg = overrides.apply(&self.base_config)?;

        // Local-first hook (minimal): the routing decision itself lives in the
        // `planner` module; this just acts on it. Runs BEFORE any chain overlay so
        // the free/local path never depends on a synced policy.
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

        // Overlay the synced on-chain `GlobalParams` policy onto this PAID job's
        // live config so fees/slashing/timing follow the authoritative on-chain
        // values (BLOCKCHAIN_ECONOMICS §12). FREE jobs are never touched and never
        // block on a chain read — the overlay only uses the already-cached policy.
        if paid && cfg.economics.enabled {
            if let Some(policy) = self.synced_policy() {
                policy.apply_to(&mut cfg.economics, &mut cfg.scheduler);
            }
        }
        let min_level = parse_level(&cfg.trust.min_attestation);
        let job_id = JobId::new();
        let query_hash = QueryHash::compute(sql, &self.engine_version);

        self.run_resilient(sql, &cfg, &job_id, &query_hash, paid, min_level, data_class)
            .await
    }

    /// The resilient re-dispatch loop (architecture §8/§11): repeatedly run one
    /// dispatch attempt, routing a stalled / silent / timed-out job to a FRESH
    /// candidate set until it completes — bounded by `max_retries` (default
    /// unlimited), bounded exponential backoff + jitter, a global retry token
    /// bucket, and an optional wall-clock cap. Fault attribution stops the loop
    /// when a consensus of nodes fails the SAME way (the query is infeasible).
    #[allow(clippy::too_many_arguments)]
    async fn run_resilient(
        &self,
        sql: &str,
        cfg: &GridConfig,
        job_id: &JobId,
        query_hash: &QueryHash,
        paid: bool,
        min_level: AttestationLevel,
        data_class: DataClass,
    ) -> Result<QueryOutcome, CoordinatorError> {
        let mut excluded: HashSet<NodeId> = HashSet::new();
        let mut backoff = Backoff::new(
            cfg.scheduler.backoff_initial_ms,
            cfg.scheduler.backoff_max_ms,
            cfg.scheduler.backoff_jitter_frac,
        );
        let mut budget = TokenBucket::new(
            cfg.scheduler.retry_budget_max_tokens,
            cfg.scheduler.retry_budget_refill_per_sec,
        );
        let started = Instant::now();
        let mut last_refill = Instant::now();
        let mut retries: u32 = 0;
        let mut last_reason = String::from("no attempt completed");

        let result = loop {
            let attempt = self
                .dispatch_attempt(
                    sql, cfg, job_id, query_hash, paid, min_level, data_class, &excluded,
                )
                .await;
            match attempt {
                Err(e) => {
                    self.progress.clear(job_id);
                    return Err(e);
                }
                Ok(AttemptResult::Done(o)) => break *o,
                Ok(AttemptResult::Infeasible { reason }) => {
                    self.progress.clear(job_id);
                    return Err(CoordinatorError::Infeasible { reason });
                }
                Ok(AttemptResult::QuorumDisagreement { agreement, quorum }) => {
                    self.progress.clear(job_id);
                    return Err(CoordinatorError::QuorumFailed { agreement, quorum });
                }
                Ok(AttemptResult::NoCandidates) => {
                    self.progress.clear(job_id);
                    if retries == 0 {
                        return Err(CoordinatorError::NoCandidates);
                    }
                    return Err(CoordinatorError::Exhausted {
                        attempts: retries + 1,
                        reason: last_reason,
                    });
                }
                Ok(AttemptResult::InsufficientWorkers { have, quorum }) => {
                    self.progress.clear(job_id);
                    if retries == 0 {
                        return Err(CoordinatorError::InsufficientWorkers { have, quorum });
                    }
                    return Err(CoordinatorError::Exhausted {
                        attempts: retries + 1,
                        reason: last_reason,
                    });
                }
                Ok(AttemptResult::Inconclusive { tried }) => {
                    last_reason = format!(
                        "attempt {} stalled/silent: {} selected provider(s) did not deliver",
                        retries + 1,
                        tried.len()
                    );
                    debug!("{last_reason}; re-dispatching to a fresh candidate set");
                    // Route around the non-delivering providers next attempt.
                    for t in tried {
                        excluded.insert(t);
                    }
                    // Stop conditions (in priority order).
                    if cfg.scheduler.max_retries != 0 && retries >= cfg.scheduler.max_retries {
                        self.progress.clear(job_id);
                        return Err(CoordinatorError::Exhausted {
                            attempts: retries + 1,
                            reason: last_reason,
                        });
                    }
                    if cfg.scheduler.max_total_duration_ms != 0
                        && started.elapsed()
                            >= Duration::from_millis(cfg.scheduler.max_total_duration_ms)
                    {
                        self.progress.clear(job_id);
                        return Err(CoordinatorError::Exhausted {
                            attempts: retries + 1,
                            reason: format!("{last_reason}; max_total_duration reached"),
                        });
                    }
                    // Global retry/hedge token bucket: refill by elapsed, then spend
                    // one token per retry. An empty bucket stops a retry storm.
                    let now = Instant::now();
                    budget.refill(now.duration_since(last_refill));
                    last_refill = now;
                    if !budget.try_take() {
                        self.progress.clear(job_id);
                        return Err(CoordinatorError::RetryBudgetExhausted {
                            attempts: retries + 1,
                        });
                    }
                    // Bounded exponential backoff + jitter before the next attempt.
                    let delay = backoff.next_delay();
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    retries += 1;
                }
            }
        };
        self.progress.clear(job_id);
        Ok(result)
    }

    /// Run ONE dispatch attempt: discover (excluding tried / convicted peers) →
    /// Offer/Bid → select top-`k` → dispatch + collect (progress-stall aware) →
    /// quorum verdict → (on success) settle + receipts + broken-commitment
    /// fining. Returns an [`AttemptResult`] the resilient loop acts on.
    #[allow(clippy::too_many_arguments)]
    async fn dispatch_attempt(
        &self,
        sql: &str,
        cfg: &GridConfig,
        job_id: &JobId,
        query_hash: &QueryHash,
        paid: bool,
        min_level: AttestationLevel,
        data_class: DataClass,
        excluded: &HashSet<NodeId>,
    ) -> Result<AttemptResult, CoordinatorError> {
        // 1. Discover a bounded candidate set, excluding tried/convicted peers.
        let filter = CandidateFilter {
            data_class,
            min_attestation: min_level,
        };
        let mut candidates = self
            .discovery
            .find_candidates(cfg.discovery.candidate_sample_size, filter)
            .await;
        let now_live = now_ms();
        candidates.retain(|c| match &c.node_id {
            Some(id) => {
                // Anti-abuse deny-list (each node independently refuses flagged actors).
                if cfg.antiabuse.enabled {
                    if let Some(bl) = &self.blocklist {
                        if bl.is_blocked(id.as_str()) {
                            return false;
                        }
                    }
                }
                // Already tried (responded / hard-failed) this job → route elsewhere.
                if excluded.contains(id) {
                    return false;
                }
                // Liveness: drop a phi-convicted peer SWIM did not rescue.
                if let Some(v) = &self.liveness {
                    if v.is_excluded(id, now_live) {
                        return false;
                    }
                }
                true
            }
            None => true, // unknown id (TOFU) can't be tracked → keep
        });
        if candidates.is_empty() {
            return Ok(AttemptResult::NoCandidates);
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
                let score = self.effective_trust(&worker, cfg, now, paid);
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

        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.2.cmp(&b.2))
        });
        scored.truncate(cfg.scheduler.replicas);

        if scored.len() < cfg.scheduler.quorum {
            return Ok(AttemptResult::InsufficientWorkers {
                have: scored.len(),
                quorum: cfg.scheduler.quorum,
            });
        }

        // 4. Dispatch to selected workers and collect commits — progress-stall
        //    aware: a streamed Progress resets the stall timer; no progress (nor a
        //    Commit) within the stall window / attempt deadline ⇒ that provider is
        //    treated as silent (job-fault, no penalty) and the job re-dispatched.
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
        let stall_ms = {
            let s = cfg.scheduler.stall_timeout_ms();
            if s == 0 {
                cfg.scheduler.dispatch_timeout_ms
            } else {
                s
            }
        };
        let stall_to = Duration::from_millis(stall_ms.max(1));
        let attempt_deadline = Duration::from_millis(cfg.scheduler.attempt_deadline_ms.max(1));
        // Idle/transfer deadline for the WINNER's result download (step 7b). A
        // silent winner must not hang the query forever; reuse the same idle
        // budget the requester already applies to commit collection.
        let result_xfer_to = stall_to;
        let selected: Vec<NodeId> = scored.iter().map(|(w, _, _)| w.clone()).collect();

        let collect_futs = scored.iter().map(|(worker, _, _)| {
            let conn = conns.get(worker).expect("selected worker has a conn");
            let dispatch = dispatch.clone();
            let worker = worker.clone();
            let progress = self.progress.as_ref();
            async move { collect_one(conn, &dispatch, worker, stall_to, attempt_deadline, progress).await }
        });

        let mut inflight: Vec<InFlight> = Vec::new();
        let mut failed: Vec<(NodeId, Verdict)> = Vec::new();
        let mut silent: Vec<NodeId> = Vec::new();
        for c in futures_util::future::join_all(collect_futs).await {
            match c {
                Collected::Committed(f) => inflight.push(f),
                Collected::Failed(node, detail) => failed.push((node, classify_failure(&detail))),
                Collected::Silent(node) => silent.push(node),
            }
        }

        // 5. Quorum decision (or canary judgement), with fault attribution to
        //    decide retry-vs-stop on a no-quorum outcome.
        let hashes: Vec<&str> = inflight.iter().map(|f| f.hash.as_str()).collect();
        let outcome = canonical::evaluate_quorum(hashes, cfg.scheduler.quorum);
        let canary_expected = self.canary.as_ref().and_then(|c| c.expected(query_hash));
        let non_verifiable =
            cfg.antiabuse.nondeterminism_active() && canary_expected.is_none() && is_nondeterministic(sql);

        // Tally how the selected providers fared (for consensus-infeasible).
        let mut tally = FaultTally::new(selected.len());
        for _ in &inflight {
            tally.record_committed();
        }
        for (_, v) in &failed {
            tally.record(*v);
        }
        for _ in &silent {
            tally.record_silent();
        }
        let frac = cfg.antiabuse.fault_attribution.job_consensus_fraction;

        let (agreed_hash, verified) = match (&canary_expected, verify_mode) {
            (Some(expected), _) => (Some(expected.clone()), true),
            _ if non_verifiable => {
                if inflight.is_empty() {
                    return Ok(AttemptResult::Inconclusive { tried: selected });
                }
                let fastest = inflight.iter().min_by_key(|f| f.latency_ms).map(|f| f.hash.clone());
                (fastest, false)
            }
            (None, VerifyMode::Quorum) => {
                if outcome.split {
                    // Equivocation: two or more distinct result hashes EACH reached
                    // quorum (`agreed_hash` is therefore already `None`). There is
                    // no single safe winner — surface it loudly instead of silently
                    // picking a side, and treat the attempt as a verification
                    // disagreement (terminal, no provider penalty).
                    tracing::warn!(
                        job = %job_id,
                        query = %query_hash.0,
                        agreement = outcome.agreement,
                        quorum = outcome.quorum,
                        "quorum SPLIT (equivocation): multiple distinct hashes each reached quorum; treating as inconclusive"
                    );
                    self.emit_failure_receipts(&inflight, job_id, query_hash, cfg);
                    return Ok(AttemptResult::QuorumDisagreement {
                        agreement: outcome.agreement,
                        quorum: outcome.quorum,
                    });
                }
                if outcome.reached() {
                    (outcome.agreed_hash.clone(), true)
                } else if cfg.antiabuse.fault_attribution_active()
                    && tally.is_consensus_infeasible(frac)
                {
                    // Consensus-infeasible query → job fault. Neutral receipts, no
                    // penalty, refund, and STOP re-dispatching.
                    let reason = tally
                        .consensus_reason(frac)
                        .unwrap_or_else(|| "consensus-infeasible query".to_string());
                    self.emit_failure_receipts(&inflight, job_id, query_hash, cfg);
                    return Ok(AttemptResult::Infeasible { reason });
                } else if inflight.len() >= cfg.scheduler.quorum {
                    // Enough providers committed but their hashes disagree — a
                    // genuine verification disagreement (terminal).
                    self.emit_failure_receipts(&inflight, job_id, query_hash, cfg);
                    return Ok(AttemptResult::QuorumDisagreement {
                        agreement: outcome.agreement,
                        quorum: outcome.quorum,
                    });
                } else {
                    // Shortfall from silence / transient failures → re-dispatch.
                    return Ok(AttemptResult::Inconclusive { tried: selected });
                }
            }
            (None, VerifyMode::Fast) => {
                if inflight.is_empty() {
                    return Ok(AttemptResult::Inconclusive { tried: selected });
                }
                let fastest = inflight.iter().min_by_key(|f| f.latency_ms).map(|f| f.hash.clone());
                (fastest, outcome.reached())
            }
        };

        let agreed = match agreed_hash {
            Some(h) => h,
            None => {
                return Ok(AttemptResult::Inconclusive { tried: selected });
            }
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
                // The authoritative answer matched no responding worker (e.g. all
                // failed a canary) — re-dispatch to fresh nodes.
                self.emit_failure_receipts(&inflight, job_id, query_hash, cfg);
                return Ok(AttemptResult::Inconclusive { tried: selected });
            }
        };

        // Requester-trust weight (ARCHITECTURE "Abuse resistance").
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
        let penalty_for = |provider_fault: bool| -> f64 {
            if provider_fault {
                (self.base_config.trust.incorrect_penalty * negative_weight).max(0.0)
            } else {
                0.0
            }
        };

        // 7. Tell winner to proceed; cancel losers (RESET). Collect the result.
        let participants: Vec<NodeId> = inflight.iter().map(|f| f.worker.clone()).collect();
        let mut result: Option<ResultSet> = None;
        let mut receipts = Vec::new();
        let winner_id = inflight[winner_idx].worker.clone();
        let mut agreeing_non_winners: Vec<NodeId> = Vec::new();
        // Providers that ACCEPTED the paid job but did NOT deliver a valid result
        // while the job was feasible — fined for the broken commitment (below).
        let mut commitment_failers: Vec<NodeId> = Vec::new();

        // Pull the winner out so every remaining `InFlight` is a loser. Order of
        // the rest is irrelevant to the verdict (it only depends on `f.hash`), so
        // a cheap `swap_remove` is fine.
        let mut winner = inflight.swap_remove(winner_idx);
        let mut losers = inflight; // renamed for clarity

        // 7a. RESET the losers IMMEDIATELY — BEFORE we await the winner's (possibly
        // large) result download. A graceful `finish()` would only close our send
        // half *after* the whole winner transfer; the loser would keep computing
        // for the entire download. Instead we abruptly QUIC-reset both directions
        // of each loser's dispatch stream (`send.reset` + `recv.stop`), which the
        // worker observes promptly and aborts on (see `worker::execute_with_progress`'s
        // cancel-watch). A best-effort `Cancel` frame is still written first so a
        // worker that happens to be blocked reading gets a clean reason; the reset
        // is the hard guarantee.
        for f in losers.iter_mut() {
            let _ = write_msg(
                &mut f.send,
                &Wire::Cancel(Cancel {
                    job_id: job_id.clone(),
                    reason: "lost race".into(),
                }),
            )
            .await;
            // Hard, immediate teardown of both directions (do NOT wait for a
            // graceful finish): stop reading the worker's result stream and reset
            // our send half so the worker's next read/write fails at once.
            let _ = f.recv.stop(LOSER_RESET_CODE);
            let _ = f.send.reset(LOSER_RESET_CODE);
        }

        // 7b. Now Ack the winner and download its result under an idle/transfer
        // deadline. A silent winner must NOT hang the whole query forever.
        let _ = write_msg(
            &mut winner.send,
            &Wire::Ack(Ack {
                job_id: job_id.clone(),
                ok: true,
                detail: "proceed".into(),
            }),
        )
        .await;
        if let Some(conn) = conns.get(&winner.worker) {
            match tokio::time::timeout(
                result_xfer_to,
                crate::result_stream::recv_result(
                    conn,
                    &mut winner.recv,
                    cfg.transport.result.max_result_bytes,
                    cfg.transport.result.max_result_parts,
                ),
            )
            .await
            {
                Ok(Ok(rs)) => result = Some(rs),
                Ok(Err(e)) => {
                    debug!(job = %job_id, worker = %winner.worker, "winner result transfer failed: {e}");
                }
                Err(_) => {
                    // Winner went silent mid/at-start of the transfer: abandon this
                    // attempt as inconclusive (job-fault, no provider penalty) and
                    // re-dispatch. Reset the stalled stream so the worker stops.
                    tracing::warn!(
                        job = %job_id,
                        worker = %winner.worker,
                        timeout_ms = result_xfer_to.as_millis() as u64,
                        "winner result transfer timed out; treating attempt as inconclusive"
                    );
                    let _ = winner.recv.stop(LOSER_RESET_CODE);
                    let _ = winner.send.reset(LOSER_RESET_CODE);
                }
            }
        }
        let _ = winner.send.finish();

        // 7c. Verdict + receipt accounting for ALL committed providers (winner +
        // losers). Streams are already settled above.
        for f in std::iter::once(&winner).chain(losers.iter()) {
            let is_winner = std::ptr::eq(f, &winner);
            if !is_winner && f.hash == agreed && !non_verifiable {
                agreeing_non_winners.push(f.worker.clone());
            }
            let verdict = if non_verifiable {
                Verdict::Inconclusive
            } else if f.hash == agreed {
                Verdict::Correct
            } else {
                Verdict::Incorrect
            };

            // A provider that committed a NON-matching hash on a verified-feasible
            // job broke its commitment (distinct from being merely slow).
            if verified && matches!(verdict, Verdict::Incorrect) {
                commitment_failers.push(f.worker.clone());
            }

            receipts.push(self.make_receipt(job_id, &f.worker, query_hash, &f.hash, verdict, f.latency_ms));
            if verdict.is_provider_fault() {
                let penalty = penalty_for(true);
                if penalty > 0.0 {
                    self.trust_store.penalize(&f.worker, penalty);
                }
            }
        }

        // Broken-commitment accounting for SELECTED providers that accepted the
        // job but never delivered a valid result, ON A FEASIBLE JOB (a verified
        // quorum was reached). A non-verified / inconclusive / infeasible job
        // never reaches here, so a query problem is never blamed on a provider.
        //
        // Two distinct cases must NOT be conflated (bid→dispatch TOCTOU):
        //  * SILENT providers (accepted the offer, then vanished without any
        //    response) genuinely broke their commitment → `Timeout` (provider
        //    fault): reputation drops and, for a paid job, they are slashed.
        //  * Providers that EXPLICITLY responded with a failure/decline at
        //    dispatch carry a CLASSIFIED verdict (`classify_failure`). A dispatch
        //    -time capacity/admission rejection ("at capacity") classifies as
        //    `Inconclusive` (neutral) — an honest, non-fault decline, NOT a broken
        //    commitment. Resource/infeasible classes are likewise blameless. We
        //    therefore honor the classified verdict and only treat it as a
        //    commitment failure (Timeout + slash) when it is an actual PROVIDER
        //    fault — never penalizing a worker that simply declined.
        if verified {
            for node in &silent {
                receipts.push(self.make_receipt(job_id, node, query_hash, "", Verdict::Timeout, 0));
                let penalty = penalty_for(true);
                if penalty > 0.0 {
                    self.trust_store.penalize(node, penalty);
                }
                commitment_failers.push(node.clone());
            }
            for (node, verdict) in &failed {
                // Use the classified verdict (neutral for an "at capacity" decline,
                // resource/infeasible for a job fault). Only an actual provider
                // fault penalizes / counts as a broken commitment.
                receipts.push(self.make_receipt(job_id, node, query_hash, "", *verdict, 0));
                if verdict.is_provider_fault() {
                    let penalty = penalty_for(true);
                    if penalty > 0.0 {
                        self.trust_store.penalize(node, penalty);
                    }
                    commitment_failers.push(node.clone());
                }
            }
        }

        for r in &receipts {
            self.trust_store.record(r);
        }
        self.trust_store.record_requester(&self_id, verified, now);

        let result = match result {
            Some(r) => r,
            None => {
                // Winner did not actually stream a result — re-dispatch.
                return Ok(AttemptResult::Inconclusive { tried: selected });
            }
        };

        // Settle the paid job: open the per-job escrow NOW that the verified quorum
        // hash is known (open-escrow-per-job, BLOCKCHAIN_ECONOMICS §6.2/§12) binding
        // the HTLC lock + synced params version, release the payout split, then
        // anchor the record. A no-op for free jobs / no rail wired.
        self.settle_paid_job(
            cfg,
            paid,
            job_id,
            &winner_id,
            &agreeing_non_winners,
            &agreed,
            query_hash,
            &self_id,
        );

        // FINE broken commitments (architecture §11): only on a PAID, feasible
        // job, only for STAKED providers. Free/local jobs and unstaked free-tier
        // providers are never fined (reputation already dropped above).
        if verified && paid && cfg.economics.enabled {
            if let Some(reg) = &self.stake_registry {
                for node in &commitment_failers {
                    self.fine_failed_commitment(reg.as_ref(), node, cfg);
                }
            }
        }

        Ok(AttemptResult::Done(Box::new(QueryOutcome {
            job_id: job_id.clone(),
            result,
            agreed_hash: Some(agreed),
            agreement: outcome.agreement,
            quorum: outcome.quorum,
            verified,
            winner: Some(winner_id),
            participants,
            receipts,
            executed_locally: false,
        })))
    }

    /// Fine a provider for a **broken commitment** on a paid, feasible job
    /// (architecture §11): slash a configurable fraction of its bonded stake via
    /// the `StakeRegistry` seam. A no-op for an unstaked provider.
    fn fine_failed_commitment(&self, reg: &dyn StakeRegistry, node: &NodeId, cfg: &GridConfig) {
        let stake = reg.stake_of(node);
        if stake == 0 {
            return; // unstaked free-tier provider → no fine (reputation only)
        }
        let amount = mul_frac(stake, cfg.economics.slashing.slash_failed_commitment_pct);
        if amount == 0 {
            return;
        }
        match reg.slash(node, SlashReason::FailedCommitment, amount) {
            Ok(()) => debug!(
                provider = %node, amount,
                "fined provider for broken commitment on a paid job"
            ),
            Err(e) => debug!("failed-commitment slash failed: {e}"),
        }
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

    /// Resolve a worker's payout wallet via the injected resolver, or a
    /// deterministic derivation (BLAKE3 of the node id) used by tests / when no
    /// binding lookup is wired.
    fn wallet_of(&self, node: &NodeId) -> WalletAddress {
        match &self.wallet_resolver {
            Some(r) => r(node),
            None => WalletAddress::new(0, *blake3::hash(node.as_str().as_bytes()).as_bytes()),
        }
    }

    /// Open the per-job escrow, release it per the quorum verdict, and anchor the
    /// job record. A no-op unless this is a PAID job on an economics-enabled node
    /// with a settlement rail wired.
    ///
    /// The escrow is opened HERE (open-escrow-per-job) — only AFTER the verified
    /// quorum hash is known — so the per-job `EscrowTerms` bind the HTLC lock
    /// (`expected_hash` = the agreed quorum result hash) AND the synced on-chain
    /// `GlobalParams` version. The payout split is bounded by the escrowed `B`,
    /// and the same params version is stamped into the anchored record
    /// (BLOCKCHAIN_ECONOMICS §6.2/§12). The mock/noop rails ignore the terms.
    #[allow(clippy::too_many_arguments)]
    fn settle_paid_job(
        &self,
        cfg: &GridConfig,
        paid: bool,
        job_id: &JobId,
        winner: &NodeId,
        agreeing_non_winners: &[NodeId],
        agreed_hash: &str,
        query_hash: &QueryHash,
        requester: &NodeId,
    ) {
        if !(paid && cfg.economics.enabled) {
            return;
        }
        let Some(settlement) = &self.settlement else {
            return;
        };
        // The HTLC lock = the agreed quorum result hash; the params version is the
        // one currently synced from on-chain `GlobalParams` (0 = unbound).
        let result_hash = *blake3::hash(agreed_hash.as_bytes()).as_bytes();
        let version = self.current_params_version();
        let max_bid = escrow_bid_nanoton(cfg);

        // Open the per-job escrow binding the lock + params version into its terms
        // (hence its deterministic address). The mock/noop rails fall back to the
        // termless `open_escrow`; the ton rail builds the on-chain terms cell.
        let handle = match settlement.open_escrow_with_terms(job_id, max_bid, &result_hash, version)
        {
            Ok(h) => h,
            Err(e) => {
                debug!("open_escrow failed: {e}");
                return;
            }
        };

        let b = handle.max_bid;
        let fee = mul_frac(b, cfg.economics.fees.platform_fee_pct);
        let commission_each = mul_frac(b, cfg.economics.fees.participation_commission_frac);
        let participants: Vec<Payout> = agreeing_non_winners
            .iter()
            .map(|n| Payout { to: self.wallet_of(n), amount: commission_each })
            .collect();
        let total_commission = commission_each.saturating_mul(participants.len() as Amount);
        // Winner takes base + perf bonus = whatever of B remains after fee +
        // commissions; never exceeds the escrow (the rail also enforces this).
        let winner_amount = b.saturating_sub(fee).saturating_sub(total_commission);
        let outcome = SettlementOutcome {
            result_hash,
            winner: Payout { to: self.wallet_of(winner), amount: winner_amount },
            participants,
            platform_fee: fee,
        };
        if let Err(e) = settlement.settle(&handle, &outcome) {
            debug!("escrow settle failed: {e}");
            return;
        }
        if let Some(anchor) = &self.record_anchor {
            anchor.append(&JobRecord {
                job_id: job_id.0.clone(),
                query_hash: query_hash.0.clone(),
                requester_wallet: self.wallet_of(requester).to_raw_string(),
                max_bid: b,
                result_hash: agreed_hash.to_string(),
                epoch: 0,
                prev_root: [0u8; 32],
                // Pin the exact on-chain params version this job ran under.
                params_version: version,
            });
        }
    }
}

/// The escrowed max bid `B` (nanoton) for a paid job: the configured
/// `[economics.pricing].max_bid` (whole TON). `0` ⇒ a 1-TON default so a paid job
/// always locks a non-zero, bounded escrow.
fn escrow_bid_nanoton(cfg: &GridConfig) -> Amount {
    const TON: Amount = 1_000_000_000;
    let whole = cfg.economics.pricing.max_bid.max(1) as Amount;
    whole * TON
}

/// Multiply a nanoton `amount` by a `[0,1]` fraction, flooring to nanoton.
fn mul_frac(amount: Amount, frac: f64) -> Amount {
    if frac <= 0.0 {
        return 0;
    }
    ((amount as f64) * frac).floor() as Amount
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

/// The per-provider outcome of one dispatch-and-collect.
enum Collected {
    /// Committed a result hash (stream left open to proceed/cancel + stream result).
    Committed(InFlight),
    /// Returned a worker error (`Ack { ok: false }`) — the detail is classified.
    Failed(NodeId, String),
    /// Went silent: no commit within the stall window / attempt deadline, the
    /// stream closed, or the host abandoned the job (over its `job_timeout`).
    Silent(NodeId),
}

/// Open a dispatch stream, send the Dispatch, then read frames until a Commit —
/// treating streamed [`Progress`] as a liveness heartbeat that resets the stall
/// timer (and updates the shared [`ProgressTracker`]). No progress nor commit
/// within `stall_to` (per read) or `attempt_deadline` (overall) ⇒ `Silent`.
async fn collect_one(
    conn: &Conn,
    dispatch: &Dispatch,
    worker: NodeId,
    stall_to: Duration,
    attempt_deadline: Duration,
    progress: &ProgressTracker,
) -> Collected {
    let inner = async {
        let (mut send, mut recv) = match conn.open_bi().await {
            Ok(x) => x,
            Err(_) => return Collected::Silent(worker.clone()),
        };
        if write_msg(&mut send, &Wire::Dispatch(dispatch.clone())).await.is_err() {
            return Collected::Silent(worker.clone());
        }
        loop {
            match tokio::time::timeout(stall_to, read_msg(&mut recv)).await {
                Ok(Ok(Wire::Progress(p))) => {
                    progress.update(p);
                    continue; // heartbeat: reset the stall timer
                }
                Ok(Ok(Wire::Commit(c))) => {
                    return Collected::Committed(InFlight {
                        worker: worker.clone(),
                        send,
                        recv,
                        hash: c.result_hash,
                        latency_ms: c.latency_ms,
                    });
                }
                Ok(Ok(Wire::Ack(a))) if !a.ok => {
                    return Collected::Failed(worker.clone(), a.detail);
                }
                // Unexpected frame, stream error, or stall timeout → silent.
                Ok(Ok(_)) | Ok(Err(_)) | Err(_) => return Collected::Silent(worker.clone()),
            }
        }
    };
    match tokio::time::timeout(attempt_deadline, inner).await {
        Ok(c) => c,
        Err(_) => Collected::Silent(worker),
    }
}