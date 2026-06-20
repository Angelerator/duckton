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
    Ack, Attestation, AttestationLevel, Bid, BidDecision, CapabilityProfile, Cancel, DataClass,
    Dispatch, InputSnapshot, JobId, NodeId, Offer, Progress, QueryHash, Receipt, ResultSet, Verdict,
    VerifyMode, Wire,
};
use p2p_settlement::{
    ensure_escrow_covers, latency_score, required_escrow_total, throughput_score, Amount, JobRecord,
    OnchainPolicy, ParamsSource, Payout, RecordAnchor, Settlement, SettlementOutcome, SlashReason,
    StakeRegistry, WalletAddress, BPS_DENOM,
};
use p2p_transport::endpoint::{read_msg, write_msg};
use p2p_transport::{Conn, NodeIdentity, QuicTransport, RecvStream, SendStream, Transport};
use p2p_trust::{
    age_factor, attestation_bound_pub, attestation_gate, canonical, classify_failure,
    exploration_bonus, is_nondeterministic, now_ts, requester_trust_weight, sign_receipt,
    soft_trust_score, AttestationVerifier, CommitKey, ReceiptDraft, TrustInputs, TrustStore,
};
use rand::Rng;
use tracing::debug;

use crate::antiabuse::Blocklist;
use crate::canary::CanaryAuditor;
use crate::discovery::{CandidateFilter, Discovery};
use crate::engine::ExecLease;
use crate::estimator::WorkingSetEstimate;
use crate::input_resolver::{InputResolveError, InputResolver};
use crate::liveness::{now_ms, LivenessView};
use crate::planner::{is_resource_exhaustion, LocalExecutor, LocalOrRemotePlanner, PlanRequest};
use crate::retry::{Backoff, FaultTally, TokenBucket};
use crate::signer::IdentitySigner;

/// QUIC application error code used when the coordinator abruptly RESETs a
/// loser's (or a stalled winner's) dispatch stream. The worker maps a reset on
/// the dispatch stream to a prompt job abort (it does not interpret the code).
const LOSER_RESET_CODE: quinn::VarInt = quinn::VarInt::from_u32(7);

/// Minimum self-measured successes before a node's PROVEN `max_input_bytes` is
/// trusted as a HARD capacity ceiling for the size gate (Part B). Below this the
/// proven record is too thin to be confident in, so it is NEVER used to exclude
/// a candidate — the cold-start-safe reading of a "confidence-shrunk proven max".
const PROVEN_GATE_MIN_SUCCESSES: u64 = 4;

/// Proven-capacity INPUT gate (size-based routing, Part B): may a job that scans
/// `scanned_bytes` be sent to a node with this (optional) gossiped proven
/// capability profile? Deliberately conservative + cold-start safe:
///  * `scanned_bytes == 0` (no/zero estimate) ⇒ always admit (route as today).
///  * No proven profile, or one with too few successes to be confident
///    (`< PROVEN_GATE_MIN_SUCCESSES`) ⇒ admit — a newcomer with no proven history
///    is never excluded here (unknown ⇒ don't exclude).
///  * Otherwise exclude ONLY when `scanned_bytes` clearly exceeds the node's
///    all-time proven `max_input_bytes` PLUS its proven spill headroom
///    (`max_temp_dir_bytes`).
fn proven_capacity_admits(profile: Option<&CapabilityProfile>, scanned_bytes: u64) -> bool {
    if scanned_bytes == 0 {
        return true;
    }
    match profile {
        Some(p) if p.successes >= PROVEN_GATE_MIN_SUCCESSES => {
            let ceiling = p.max_input_bytes.saturating_add(p.max_temp_dir_bytes);
            scanned_bytes <= ceiling
        }
        // Unknown / thin proven history ⇒ never exclude (cold-start safety).
        _ => true,
    }
}

/// Advertised-memory capacity gate (size-based routing, Part B): may a job whose
/// estimated peak working set is `peak_bytes` be sent to a node that advertised
/// `free_mem_bytes` in its bid (optionally extended by a PROVEN sustained peak)?
/// Conservative + cold-start safe:
///  * `peak_bytes == 0` (no/zero estimate) ⇒ always admit.
///  * `free_mem_bytes == 0` (unknown / older peer) ⇒ admit (do not gate on what
///    we do not know).
///  * Otherwise capacity = `free_mem + spill_tolerance`, raised to any proven
///    sustained peak (`max_peak_memory_bytes + max_temp_dir_bytes`); exclude only
///    when `peak_bytes` exceeds that capacity.
fn advertised_capacity_admits(
    free_mem_bytes: u64,
    profile: Option<&CapabilityProfile>,
    peak_bytes: u64,
    spill_tolerance: u64,
) -> bool {
    if peak_bytes == 0 || free_mem_bytes == 0 {
        return true;
    }
    let mut capacity = free_mem_bytes.saturating_add(spill_tolerance);
    if let Some(p) = profile {
        capacity = capacity.max(p.max_peak_memory_bytes.saturating_add(p.max_temp_dir_bytes));
    }
    peak_bytes <= capacity
}

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
        "query exceeds the capacity of all available nodes — a consensus of selected providers \
         ran out of resources (OOM / too big) and no higher-capacity node is available to \
         re-route to: {reason}"
    )]
    ExceedsCapacity { reason: String },
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
    /// Up-front escrow coverage failure (time-based pricing): the requester's
    /// maxEscrow / balance cannot cover the worst-case metered settlement
    /// (`cap_base + φ platform fee + κ commission × N runners`) for the selected
    /// providers. The job is REJECTED before any dispatch rather than failing
    /// mid-settle. The message is the human-readable insufficient-escrow text.
    #[error("{0}")]
    InsufficientEscrow(String),
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
    /// (the query is infeasible — bad SQL) — STOP (job fault, no provider
    /// penalty). Terminal: retrying cannot help.
    Infeasible { reason: String },
    /// A consensus of selected providers ran OUT OF RESOURCES (OOM / the job was
    /// too big for them). NOT terminal: exclude the OOMed nodes and re-route to
    /// other, higher-capacity nodes. `oomed` are the providers to exclude;
    /// `max_tried_capacity` is the largest free-memory capacity that already
    /// failed (so the loop only keeps going while strictly bigger nodes exist).
    ResourceExceeded {
        oomed: Vec<NodeId>,
        max_tried_capacity: u64,
        reason: String,
    },
    /// Enough providers committed but their result hashes did not agree — a
    /// genuine verification disagreement (terminal).
    QuorumDisagreement { agreement: usize, quorum: usize },
    /// One or more providers read a DIFFERENT input snapshot than the one pinned
    /// at dispatch (the external source data changed between replica executions).
    /// This is benign — re-pin a fresh snapshot and re-dispatch, NEVER penalizing
    /// the honest minority that read the newer bytes (deterministic-input
    /// verification). Retried like `Inconclusive`, but without excluding anyone.
    InputDrift { reason: String },
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
    /// grid jobs that resolve to PAID while `economics.enabled`. The escrow's
    /// HTLC lock binds the agreed quorum result hash, so it is opened (funded with
    /// the max bid) and released per the verdict *post-quorum*; both the open and
    /// the settle are now on-chain **confirmed** (not fire-and-forget — see
    /// `p2p_settlement`'s `await_confirmation`). True pre-dispatch bid-locking
    /// would require decoupling the lock from the result hash in `JobEscrow` (a
    /// contract change). `None` (default) and FREE jobs never touch it.
    settlement: Option<Arc<dyn Settlement>>,
    /// Optional tamper-proof record anchor (§7): a settled paid job's `JobRecord`
    /// is appended to the off-chain epoch tree (root anchored on-chain elsewhere).
    record_anchor: Option<Arc<dyn RecordAnchor>>,
    /// Resolves a worker `NodeId` to its bound payout wallet. Defaults to a
    /// deterministic derivation; the live wiring injects the real node↔wallet
    /// binding lookup.
    wallet_resolver: Option<Arc<dyn Fn(&NodeId) -> WalletAddress + Send + Sync>>,
    /// FREE-NODE POLICY: resolves whether a worker has a real payout WALLET binding
    /// (`Some(wallet)`) or is a free/walletless node (`None`). When set, it is the
    /// authority for payout decisions: a free winner is paid `0` (the base refunds
    /// to the requester) and only wallet-holding verifiers earn the κ commission —
    /// while the platform still collects φ·base. When `None` (default) every node is
    /// treated as a wallet node (via `wallet_resolver`/derivation), exactly today's
    /// behavior, so paid jobs with all-wallet nodes are unchanged.
    wallet_binding: Option<Arc<dyn Fn(&NodeId) -> Option<WalletAddress> + Send + Sync>>,
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
    /// Optional attestation verifier (architecture §7.2/§9.3). When `None`
    /// (default), a bid's self-reported attestation level is NOT trusted above L0
    /// — any `> L0` claim is treated as L0 (fail closed), so a spoofed level can
    /// never satisfy a `> L0` (data-class) gate. When wired, a `> L0` claim is
    /// honored ONLY if its evidence verifies (trusted-authority signature over the
    /// allowlisted measurement + this offer's nonce). Honest L0 hosts (all shipped
    /// hosts) are unaffected — L0 needs no evidence.
    attestation_verifier: Option<Arc<dyn AttestationVerifier>>,
    /// Per-job scoped storage credential issuer (architecture §9.2). When wired
    /// AND a storage provider is configured, the coordinator mints a short-lived,
    /// prefix-scoped credential and attaches it to each [`Dispatch`]. `None`
    /// (default) ⇒ `Dispatch.credential = None`, the unchanged local-data path.
    credential_provider: Option<Arc<dyn crate::storage::StorageCredentialProvider>>,
    /// Per-object presigned-URL signer (presigned credential mode, architecture
    /// §9.2). When wired AND the job's inputs are pinned, the coordinator signs a
    /// short-TTL read URL per pinned object and ships them in `Dispatch.signed_inputs`
    /// so the worker reads via plain HTTPS with NO secret on the host. `None`
    /// (default) ⇒ no presigning (the secret-based path, or no remote access).
    presign_provider: Option<Arc<dyn crate::storage::PresignProvider>>,
    /// How a worker is granted read access to the pinned objects (presigned /
    /// scoped-secret / sealed). Mirrors `storage.credential_mode`; selects between
    /// the presign path and the scoped-credential path at dispatch time.
    credential_mode: p2p_config::CredentialMode,
    /// Wallet↔node binding source (architecture §3.2 / G4). When wired, selection
    /// reputation is AGGREGATED across every node id bound to the same collateral
    /// wallet — so rotating the node key alone (re-binding the new id to the same
    /// wallet) does NOT shed accumulated history. `None` (default) ⇒ per-node
    /// reputation, exactly as before.
    binding_store: Option<Arc<p2p_config::BindingStore>>,
    /// Optional input resolver (deterministic-input verification). When wired,
    /// the coordinator pins a version-identified snapshot of the job's external
    /// inputs at dispatch time and attaches it to each `Dispatch`, so "the source
    /// data changed between replicas" (benign drift) is distinguishable from "a
    /// provider returned a wrong result" (fault). `None` (default) ⇒ no pinning:
    /// verification falls back to result-hash quorum, exactly today's behavior.
    input_resolver: Option<Arc<dyn InputResolver>>,
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

/// Requester-measured workload magnitude attested into a [`Receipt`] (the
/// grid-wide measured-capability signal). `0` = unknown. Only the winner's
/// result is measured by the requester (losers are reset before transfer), so
/// non-winner / failure receipts carry the default zeros.
#[derive(Debug, Clone, Copy, Default)]
struct ObservedSize {
    input_bytes: u64,
    result_rows: u64,
    result_bytes: u64,
}

/// Bytes in one GiB (the unit for the optional metered per-GB byte term).
const GIB_BYTES: Amount = 1024 * 1024 * 1024;

/// Nanoton in one whole TON (config prices `max_bid`/`unit_price` are whole TON).
const TON_NANOTON: Amount = 1_000_000_000;

/// The time-based (usage) pricing terms carried by a worker's [`Bid`]: the
/// per-second / per-GiB rates and the billing/execution `cap_seconds`. Present
/// only when the bid advertised a non-zero rate (the metered model); otherwise
/// the coordinator uses the fixed-price path (today's behavior).
#[derive(Debug, Clone, Copy)]
struct MeteredTerms {
    /// Per-second rate in nanoton.
    rate_per_second: Amount,
    /// Optional per-GiB rate in nanoton.
    rate_per_gb: Amount,
    /// Billing ceiling AND hard execution deadline (seconds).
    cap_seconds: u64,
}

impl MeteredTerms {
    /// Extract the metered terms from a bid, or `None` when it carries no rate
    /// (an older peer / a fixed-price bid).
    fn from_bid(bid: &Bid) -> Option<Self> {
        if bid.rate_per_second == 0 {
            return None;
        }
        Some(Self {
            rate_per_second: bid.rate_per_second as Amount,
            rate_per_gb: bid.rate_per_gb as Amount,
            cap_seconds: bid.cap_seconds.max(1),
        })
    }

    /// The optional per-GiB byte term in nanoton for `bytes` scanned input.
    fn bytes_term(&self, bytes: u64) -> Amount {
        self.rate_per_gb.saturating_mul(bytes as Amount) / GIB_BYTES
    }

    /// Worst-case `cap_base = rate × cap_seconds (+ byte term)` — the most the job
    /// can cost, used to size/verify the escrow up front.
    fn cap_base(&self, bytes: u64) -> Amount {
        self.rate_per_second
            .saturating_mul(self.cap_seconds as Amount)
            .saturating_add(self.bytes_term(bytes))
    }

    /// The settled `base = rate × min(billed_seconds, cap_seconds) (+ byte term)`.
    fn base_for(&self, billed_seconds: u64, bytes: u64) -> Amount {
        let secs = billed_seconds.min(self.cap_seconds) as Amount;
        self.rate_per_second
            .saturating_mul(secs)
            .saturating_add(self.bytes_term(bytes))
    }
}

/// The fully-resolved metered settlement inputs for the WINNER, computed once the
/// quorum verdict + measured latencies are known. `None` ⇒ the fixed-price path.
#[derive(Debug, Clone, Copy)]
struct MeteredSettle {
    terms: MeteredTerms,
    /// The cross-checked billed seconds (`min(winner, median × tolerance)`).
    billed_seconds: u64,
    /// The estimator's scanned-bytes estimate (the optional byte term basis).
    bytes: u64,
}

/// The price signal used to rank a bid (lower = cheaper ⇒ ranked higher): the
/// per-second rate under the metered model, else the fixed `price`.
fn bid_price_signal(bid: &Bid) -> u64 {
    if bid.rate_per_second > 0 {
        bid.rate_per_second
    } else {
        bid.price
    }
}

/// Min-max normalize a price into a `[0,1]` score where CHEAPER ⇒ higher. All
/// candidates equal (or a single candidate) ⇒ neutral `1.0`.
fn normalize_price(price: u64, pmin: u64, pmax: u64) -> f64 {
    if pmax <= pmin {
        return 1.0;
    }
    1.0 - (price.saturating_sub(pmin) as f64 / (pmax - pmin) as f64)
}

/// Reliability gate for the stake ranking term (verified-success-rate guardrail).
///
/// Returns the `[0,1]` factor the raw `stake_factor` is multiplied by before it
/// enters the composite selection score. It ramps linearly from `0` at `floor`
/// to `1` at `reliability == 1.0`, and is `0` for any reliability at/below the
/// floor. `reliability` is the node's STAKE-INDEPENDENT confidence-aware
/// reputation (its Wilson-shrunk verified-success rate), so:
///
///   * stake AMPLIFIES the ranking of nodes that are already reliable, and
///   * a low-reputation node earns ~no stake credit, so it cannot climb above a
///     reliable node by staking more (it cannot lower the first-try
///     verified-success rate),
///   * extra stake cannot inflate the gate itself (the input excludes stake).
///
/// `floor == 0` ⇒ the factor is exactly `reliability` (stake still scaled by
/// reliability, no dead-zone). `floor >= 1` ⇒ stake earns credit only at a
/// perfect reputation.
fn stake_reliability_factor(reliability: f64, floor: f64) -> f64 {
    let floor = floor.clamp(0.0, 1.0);
    let r = reliability.clamp(0.0, 1.0);
    if floor >= 1.0 {
        return if r >= 1.0 { 1.0 } else { 0.0 };
    }
    ((r - floor) / (1.0 - floor)).clamp(0.0, 1.0)
}

/// Weight-normalized composite selection blend (pure; the math behind
/// [`Coordinator::selection_score`]). The `stake` term is reliability-GATED:
/// it is multiplied by [`stake_reliability_factor`] of the node's stake-free
/// `reliability` so a higher stake only ever amplifies an already-reliable node
/// and never rescues a low-reputation one. With all weights `0` the score
/// collapses to `trust` (today's ranking).
#[allow(clippy::too_many_arguments)]
fn blend_selection_score(
    trust: f64,
    lat: f64,
    thr: f64,
    stake: f64,
    reliability: f64,
    price_score: f64,
    r: &p2p_config::RankingEconomics,
) -> f64 {
    let gated_stake = stake * stake_reliability_factor(reliability, r.stake_reliability_floor);
    let wsum = r.w_quality + r.w_latency + r.w_throughput + r.w_stake + r.w_price;
    if wsum <= 0.0 {
        return trust;
    }
    ((r.w_quality * trust
        + r.w_latency * lat
        + r.w_throughput * thr
        + r.w_stake * gated_stake
        + r.w_price * price_score)
        / wsum)
        .clamp(0.0, 1.0)
}

/// Median of a slice of latencies (ms); `0` for an empty slice.
fn median_ms(latencies: &[u64]) -> u64 {
    if latencies.is_empty() {
        return 0;
    }
    let mut v = latencies.to_vec();
    v.sort_unstable();
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2
    }
}

/// `ceil(ms / 1000)` — whole processing seconds from a millisecond latency.
fn ceil_secs(ms: u64) -> u64 {
    ms.div_ceil(1000)
}

/// The metered **billed seconds**: the requester-observed winner commit latency
/// (`processing_seconds = ceil(ms/1000)`), cross-checked against the quorum
/// verifiers' median latency so a single slow/over-reporting winner cannot
/// over-bill — billed = `min(winner_seconds, ceil(median × tolerance))`.
fn metered_billed_seconds(winner_ms: u64, agreeing_ms: &[u64], tolerance: f64) -> u64 {
    let winner_secs = ceil_secs(winner_ms);
    let median = median_ms(agreeing_ms);
    if median == 0 {
        return winner_secs;
    }
    let median_cap_secs = ceil_secs((median as f64 * tolerance.max(1.0)).ceil() as u64);
    winner_secs.min(median_cap_secs)
}

/// One worker that committed a result hash and whose decision stream is open.
struct InFlight {
    worker: NodeId,
    send: SendStream,
    recv: RecvStream,
    hash: String,
    latency_ms: u64,
    /// The input-snapshot fingerprint the worker reported reading (empty when the
    /// worker reported none — an older peer / unpinned job).
    input_fingerprint: String,
}

/// The per-query request-scoping constraints the coordinator stamps into each
/// `Offer` and applies during candidate selection (architecture §7.5). Computed
/// once per query from the per-call [`QueryOverrides`] + this node's
/// `[membership]` claims. All-empty/`None` ⇒ no constraint = today's behavior.
#[derive(Debug, Clone, Default)]
struct RequestScope {
    /// Target logical partition (`None` ⇒ any).
    network: Option<String>,
    /// The requester's claimed group memberships (its `[membership].groups` unless
    /// the call overrode them) — presented to hosts for the group-match check.
    groups: Vec<String>,
    /// Regions the requester will accept a host in (empty ⇒ no constraint).
    regions: Vec<String>,
    /// The requester's group-membership proof (JSON `CapabilityToken`), stamped
    /// into the Offer so token-tier hosts can verify it. `None` under soft tier.
    group_proof: Option<String>,
}

impl Coordinator {
    pub fn new(
        transport: Arc<QuicTransport>,
        discovery: Arc<dyn Discovery>,
        trust_store: Arc<dyn TrustStore>,
        base_config: Arc<GridConfig>,
        engine_version: impl Into<String>,
    ) -> Self {
        let credential_mode = base_config.storage.credential_mode;
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
            credential_provider: None,
            presign_provider: None,
            credential_mode,
            binding_store: None,
            settlement: None,
            record_anchor: None,
            wallet_resolver: None,
            wallet_binding: None,
            blocklist: None,
            liveness: None,
            progress: Arc::new(ProgressTracker::new()),
            synced_params: Arc::new(SyncedParams::default()),
            params_source: None,
            attestation_verifier: None,
            input_resolver: None,
        }
    }

    /// Wire an input resolver (deterministic-input verification). When set, the
    /// coordinator pins a version-identified snapshot of each job's external
    /// inputs at dispatch time, so a benign "data changed between replicas"
    /// outcome is never mis-attributed as a provider fault. Off by default ⇒ no
    /// pinning (result-hash quorum exactly as before).
    pub fn with_input_resolver(mut self, resolver: Arc<dyn InputResolver>) -> Self {
        self.input_resolver = Some(resolver);
        self
    }

    /// Wire an attestation verifier so a bid claiming `> L0` is honored ONLY with
    /// VERIFIED evidence (architecture §7.2/§9.3). Off by default ⇒ every `> L0`
    /// self-report is downgraded to L0 (fail closed); honest L0 hosts unaffected.
    pub fn with_attestation_verifier(mut self, verifier: Arc<dyn AttestationVerifier>) -> Self {
        self.attestation_verifier = Some(verifier);
        self
    }

    /// Wire a per-job scoped-credential issuer (architecture §9.2). When set, each
    /// [`Dispatch`] carries a short-lived credential scoped to the job's prefix so
    /// the worker can mint a read-only `CREATE SECRET`. Off by default ⇒ no
    /// credential is attached (unchanged local-data path).
    pub fn with_credential_provider(
        mut self,
        provider: Arc<dyn crate::storage::StorageCredentialProvider>,
    ) -> Self {
        self.credential_provider = Some(provider);
        self
    }

    /// Wire a per-object presigned-URL signer (presigned credential mode,
    /// architecture §9.2). When set AND the job's inputs are pinned, each pinned
    /// object is signed into a short-TTL read URL shipped in
    /// [`Dispatch::signed_inputs`], so the worker reads via plain HTTPS with NO
    /// `CREATE SECRET` / no reusable credential on the host. Off by default ⇒ the
    /// scoped-credential path (or no remote access).
    pub fn with_presign_provider(
        mut self,
        provider: Arc<dyn crate::storage::PresignProvider>,
    ) -> Self {
        self.presign_provider = Some(provider);
        self
    }

    /// Override the credential mode (presigned / scoped-secret / sealed). Defaults
    /// from `storage.credential_mode`; exposed for explicit wiring/tests.
    pub fn with_credential_mode(mut self, mode: p2p_config::CredentialMode) -> Self {
        self.credential_mode = mode;
        self
    }

    /// Wire a wallet↔node binding source so reputation is aggregated by collateral
    /// wallet (G4): a key rotation that re-binds to the same wallet inherits the
    /// wallet's history. Off by default ⇒ per-node reputation (unchanged).
    pub fn with_binding_store(mut self, store: Arc<p2p_config::BindingStore>) -> Self {
        self.binding_store = Some(store);
        self
    }

    /// Node ids that share `worker`'s collateral wallet (incl. `worker`). Empty
    /// when no binding source is wired or `worker` has no recorded binding ⇒
    /// callers fall back to per-node behavior.
    fn wallet_siblings(&self, worker: &NodeId) -> Vec<NodeId> {
        let Some(bs) = &self.binding_store else {
            return Vec::new();
        };
        let Ok(Some(entry)) = bs.get(worker.as_str()) else {
            return Vec::new();
        };
        match bs.list() {
            Ok(all) => all
                .into_iter()
                .filter(|e| e.wallet == entry.wallet)
                .map(|e| NodeId(e.node_id))
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Confidence-aware reputation + observation count for selection, AGGREGATED
    /// across the worker's collateral-wallet siblings when a binding source is
    /// wired (observation-weighted mean + summed observations). Falls back to the
    /// per-node values when there is no binding (unchanged behavior).
    fn wallet_reputation(&self, worker: &NodeId, cfg: &GridConfig, now: u64) -> (f64, usize) {
        let rep = &cfg.economics.reputation;
        let confident = |id: &NodeId| {
            self.trust_store.confident_reputation(
                id,
                now,
                rep.prior_alpha,
                rep.prior_beta,
                rep.confidence_z,
            )
        };
        let siblings = self.wallet_siblings(worker);
        if siblings.len() <= 1 {
            // No binding (or a lone id on the wallet): per-node, exactly as before.
            let reputation = confident(worker).unwrap_or(cfg.trust.bootstrap_trust);
            return (reputation, self.trust_store.observation_count(worker));
        }
        let mut weighted = 0.0;
        let mut total = 0usize;
        for sib in &siblings {
            let obs = self.trust_store.observation_count(sib);
            if obs == 0 {
                continue;
            }
            if let Some(r) = confident(sib) {
                weighted += r * obs as f64;
                total += obs;
            }
        }
        if total == 0 {
            (cfg.trust.bootstrap_trust, 0)
        } else {
            (weighted / total as f64, total)
        }
    }

    /// The **verified** attestation level to use for selection: a self-reported
    /// level at or below L0 is taken as-is (no evidence needed), but a claim ABOVE
    /// L0 is honored only if a verifier is wired AND the evidence validates against
    /// this offer's `nonce` (trusted-authority signature over the allowlisted
    /// measurement + nonce + bound key). Otherwise the host is treated as L0, so a
    /// spoofed level cannot pass a `> L0` gate. This replaces the former
    /// trust-the-integer compare on the self-reported level.
    fn verified_attestation_level(&self, att: &Attestation, nonce: &[u8]) -> AttestationLevel {
        if att.level <= AttestationLevel::L0 {
            return att.level;
        }
        match &self.attestation_verifier {
            Some(v) => {
                let bound = attestation_bound_pub(att).unwrap_or([0u8; 32]);
                if v.verify(att, nonce, &bound).is_ok() {
                    att.level
                } else {
                    AttestationLevel::L0
                }
            }
            None => AttestationLevel::L0,
        }
    }

    /// Region attested tier: verify a bidder's `region_proof` against the trusted
    /// region issuer for one of the requester's accepted regions, binding the
    /// proven holder key to the worker's node id. Any parse/verify/binding failure
    /// ⇒ not attested (fail closed).
    fn region_proof_ok(
        &self,
        worker: &NodeId,
        bid: &Bid,
        regions: &[String],
        issuers: &std::collections::BTreeMap<String, String>,
    ) -> bool {
        let Some(proof) = &bid.region_proof else {
            return false;
        };
        let Ok(token) = serde_json::from_str::<p2p_trust::CapabilityToken>(proof) else {
            return false;
        };
        let now = now_ts();
        for region in regions {
            let Some(issuer_hex) = issuers.get(region) else {
                continue;
            };
            let Some(issuer) = hex::decode(issuer_hex)
                .ok()
                .and_then(|b| <[u8; 32]>::try_from(b).ok())
            else {
                continue;
            };
            if let Ok(holder) = p2p_trust::verify_region_attestation(&token, &issuer, region, now) {
                if NodeId::from_pubkey(&holder) == *worker {
                    return true;
                }
            }
        }
        false
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

    /// The authoritative platform-fee RECIPIENT (admin treasury) from the cached
    /// on-chain `GlobalParams` policy, or `None` when nothing has been synced (no
    /// params source wired). This is the value an honest paid job binds as the
    /// escrow `treasury`; local `economics.fee_recipient` is only an optional
    /// cross-check that must match it.
    pub fn current_fee_recipient(&self) -> Option<WalletAddress> {
        self.synced_policy().map(|p| p.fee_recipient)
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

    /// FREE-NODE POLICY: wire a `NodeId → Option<payout wallet>` binding resolver
    /// that distinguishes WALLET nodes (`Some`) from FREE/walletless nodes (`None`).
    /// With it, a free node may fully participate in — and even WIN — a paid job: a
    /// free winner is paid `0` and its base refunds to the requester, while the
    /// platform still collects φ·base and wallet-holding verifiers earn κ·base.
    /// Without it, every node is treated as a wallet node (today's behavior).
    pub fn with_wallet_binding(
        mut self,
        resolver: Arc<dyn Fn(&NodeId) -> Option<WalletAddress> + Send + Sync>,
    ) -> Self {
        self.wallet_binding = Some(resolver);
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
        let cfg = overrides.apply(&self.base_config)?;

        // Local-first hook (minimal): the routing decision itself lives in the
        // `planner` module; this just acts on it. Runs BEFORE any chain overlay so
        // the free/local path never depends on a synced policy.
        if let Some(outcome) = self.try_local_execution(sql, &cfg, None).await? {
            return Ok(outcome);
        }

        // No pre-flight estimate on this entry point ⇒ the per-job observed input
        // size stays unknown (`0`). The estimate-aware entry is
        // [`Coordinator::run_query_planned`].
        self.dispatch_to_grid(sql, cfg, &overrides, None, None).await
    }

    /// Dispatch a query to the grid (the non-local path shared by
    /// [`Coordinator::run_query`] and [`Coordinator::run_query_planned`]).
    /// `estimated_input_bytes` is the estimator's scanned-bytes estimate threaded
    /// through to the per-job observed-size accounting (`None`/`0` = unknown);
    /// `estimated_peak_bytes` is the estimated peak working set used by the
    /// size-based capability gate.
    async fn dispatch_to_grid(
        &self,
        sql: &str,
        mut cfg: GridConfig,
        overrides: &QueryOverrides,
        estimated_input_bytes: Option<u64>,
        estimated_peak_bytes: Option<u64>,
    ) -> Result<QueryOutcome, CoordinatorError> {
        // Per-job data class from the request (`data_class => ...`), defaulting to
        // Public so the unset path is unchanged. It drives the worker admission
        // gate (`serves_data_class`), the economics free/paid resolution, and the
        // attestation/min-trust selection floors already applied in `apply`.
        let data_class = data_class_from_cfg(overrides.data_class.unwrap_or(DataClassCfg::Public));
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

        // Request-scoping constraints (§7.5): the per-call network/region target +
        // the requester's group claims (per-call `groups` override, else the node's
        // own `[membership].groups`). All-empty ⇒ no constraint (unchanged routing).
        let scope = RequestScope {
            network: overrides.network.clone(),
            groups: if overrides.groups.is_empty() {
                cfg.membership.groups.clone()
            } else {
                overrides.groups.clone()
            },
            regions: overrides.regions.clone(),
            group_proof: cfg.membership.group_token.clone(),
        };

        self.run_resilient(
            sql,
            &cfg,
            &job_id,
            &query_hash,
            paid,
            min_level,
            data_class,
            &scope,
            estimated_input_bytes,
            estimated_peak_bytes,
        )
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
        scope: &RequestScope,
        estimated_input_bytes: Option<u64>,
        estimated_peak_bytes: Option<u64>,
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
        // Capacity-aware re-route state (Part A.1): once a consensus of nodes OOMs
        // ("too big"), we exclude them and only re-route to STRICTLY higher-capacity
        // nodes (`capacity_floor` = the largest free-memory capacity that already
        // failed). If a subsequent attempt then finds no candidates, the failure is
        // terminal `ExceedsCapacity` rather than a generic exhaustion.
        let mut capacity_floor: u64 = 0;
        let mut saw_resource_exceeded = false;

        let result = loop {
            let attempt = self
                .dispatch_attempt(
                    sql,
                    cfg,
                    job_id,
                    query_hash,
                    paid,
                    min_level,
                    data_class,
                    scope,
                    &excluded,
                    estimated_input_bytes,
                    estimated_peak_bytes,
                    capacity_floor,
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
                    // A consensus of nodes already OOMed and we then exhausted the
                    // higher-capacity candidates → the job exceeds every available
                    // node's capacity (clear, actionable error).
                    if saw_resource_exceeded {
                        return Err(CoordinatorError::ExceedsCapacity { reason: last_reason });
                    }
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
                    if saw_resource_exceeded {
                        return Err(CoordinatorError::ExceedsCapacity { reason: last_reason });
                    }
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
                Ok(AttemptResult::ResourceExceeded {
                    oomed,
                    max_tried_capacity,
                    reason,
                }) => {
                    // Consensus OOM ("too big") → NOT terminal. Exclude the OOMed
                    // nodes, raise the capacity floor so the next attempt only
                    // considers STRICTLY higher-capacity nodes, and re-route. Only
                    // when no higher-capacity candidate remains does the loop fall
                    // through to `NoCandidates`/`InsufficientWorkers`, which (because
                    // `saw_resource_exceeded` is set) surface as `ExceedsCapacity`.
                    last_reason = format!("attempt {} resource-exceeded: {reason}", retries + 1);
                    debug!("{last_reason}; excluding OOMed nodes and re-routing to higher-capacity nodes");
                    saw_resource_exceeded = true;
                    capacity_floor = capacity_floor.max(max_tried_capacity);
                    for n in oomed {
                        excluded.insert(n);
                    }
                    // Same stop conditions as the Inconclusive path.
                    if cfg.scheduler.max_retries != 0 && retries >= cfg.scheduler.max_retries {
                        self.progress.clear(job_id);
                        return Err(CoordinatorError::ExceedsCapacity { reason: last_reason });
                    }
                    if cfg.scheduler.max_total_duration_ms != 0
                        && started.elapsed()
                            >= Duration::from_millis(cfg.scheduler.max_total_duration_ms)
                    {
                        self.progress.clear(job_id);
                        return Err(CoordinatorError::ExceedsCapacity {
                            reason: format!("{last_reason}; max_total_duration reached"),
                        });
                    }
                    let now = Instant::now();
                    budget.refill(now.duration_since(last_refill));
                    last_refill = now;
                    if !budget.try_take() {
                        self.progress.clear(job_id);
                        return Err(CoordinatorError::RetryBudgetExhausted {
                            attempts: retries + 1,
                        });
                    }
                    let delay = backoff.next_delay();
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    retries += 1;
                }
                Ok(AttemptResult::InputDrift { reason }) => {
                    // Benign input drift (deterministic-input verification): the
                    // source data changed between replica executions. Re-pin a
                    // fresh snapshot and re-dispatch WITHOUT excluding anyone — the
                    // honest minority that read newer bytes is not at fault. The
                    // re-resolve happens at the top of the next `dispatch_attempt`.
                    last_reason = format!("attempt {} input drift: {reason}", retries + 1);
                    debug!("{last_reason}; re-pinning a fresh snapshot and re-dispatching (no penalty)");
                    // Same stop conditions as the Inconclusive path (max_retries,
                    // wall-clock cap, retry token bucket) to bound a flapping source.
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
                    let now = Instant::now();
                    budget.refill(now.duration_since(last_refill));
                    last_refill = now;
                    if !budget.try_take() {
                        self.progress.clear(job_id);
                        return Err(CoordinatorError::RetryBudgetExhausted {
                            attempts: retries + 1,
                        });
                    }
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
        scope: &RequestScope,
        excluded: &HashSet<NodeId>,
        estimated_input_bytes: Option<u64>,
        estimated_peak_bytes: Option<u64>,
        capacity_floor: u64,
    ) -> Result<AttemptResult, CoordinatorError> {
        // 0. Deterministic-input verification: pin a version-identified snapshot
        //    of this job's external inputs RIGHT NOW (re-resolved per attempt, so
        //    the TOCTOU window between pinning and dispatch stays small). `None`
        //    resolver / no pinnable source ⇒ no pin (today's behavior); a source
        //    that cannot be statically pinned (dynamic SQL) ⇒ treat the job as
        //    non-verifiable (no penalty); an unreachable source ⇒ Infeasible.
        let (input_snapshot, inputs_not_pinnable) = match &self.input_resolver {
            Some(resolver) => match resolver.resolve(sql).await {
                Ok(snap) => (snap, false),
                Err(InputResolveError::NotPinnable(reason)) => {
                    debug!(job = %job_id, "inputs not statically pinnable ({reason}); running non-verifiable (no penalty)");
                    (None, true)
                }
                Err(InputResolveError::Unavailable(reason)) => {
                    return Ok(AttemptResult::Infeasible {
                        reason: format!("input source unavailable: {reason}"),
                    });
                }
            },
            None => (None, false),
        };
        let pinned_fingerprint = input_snapshot.as_ref().map(|s| s.fingerprint.clone());

        // 1. Discover a bounded candidate set, excluding tried/convicted peers.
        let filter = CandidateFilter {
            data_class,
            min_attestation: min_level,
            network: scope.network.clone(),
            groups: scope.groups.clone(),
            regions: scope.regions.clone(),
            // Private mode (§ closure): network/group labels fail closed — an
            // unknown-labeled candidate is dropped, not kept on the soft
            // assumption the host re-checks. Public mode keeps today's soft prune.
            fail_closed_labels: cfg.is_private(),
        };
        let mut candidates = self
            .discovery
            .find_candidates(cfg.discovery.candidate_sample_size, filter.clone())
            .await;
        let now_live = now_ms();
        candidates.retain(|c| {
            // Request-scoping (§7.5): network/group are SOFT (kept on unknown
            // labels — the host re-checks at admission); region is fail-closed (an
            // unknown region is dropped, mirroring `require_staked_hosts`). Applies
            // to both known-id and TOFU candidates (it reads advertised labels).
            // Same matcher discovery uses, so it also catches a `Discovery` impl
            // that didn't prune.
            if !filter.admits_labels(c) {
                return false;
            }
            // TOFU fail-closed (anti-Sybil): the stake/sybil gates only trust a
            // cryptographically attributable id (operator-pinned or signed-ad). A
            // trust-on-first-use id — even one pinned for routing/dedup — is NOT
            // gate-eligible, so learning-then-pinning an id can never relax these
            // gates. With the gates off, provenance is irrelevant (unchanged).
            let gates_on = cfg.scheduler.require_staked_hosts || cfg.sybil.min_stake > 0;
            if gates_on && !c.id_provenance.is_verified() {
                return false;
            }
            match &c.node_id {
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
                // Size-based capability gate (Part B): drop a candidate whose
                // gossiped PROVEN capacity clearly cannot handle this job's scanned
                // bytes. Cold-start safe — a node with no (or thin) proven history
                // is NEVER excluded here (see `proven_capacity_admits`). The free-mem
                // gate (which needs the bid) is applied after bidding below. Only
                // consult the (locked) capability cache when there is an estimate —
                // the no-estimate path stays byte-for-byte unchanged.
                let scanned = estimated_input_bytes.unwrap_or(0);
                if scanned > 0
                    && !proven_capacity_admits(self.discovery.proven_capacity(id).as_ref(), scanned)
                {
                    return false;
                }
                // Liveness: drop a phi-convicted peer SWIM did not rescue.
                if let Some(v) = &self.liveness {
                    if v.is_excluded(id, now_live) {
                        return false;
                    }
                }
                // Staked-hosts security gate: when required, only bonded hosts
                // (positive stake in the wired registry) qualify. No registry
                // wired ⇒ nobody qualifies (fail closed → NoCandidates).
                if cfg.scheduler.require_staked_hosts {
                    let staked = self
                        .stake_registry
                        .as_ref()
                        .map(|r| r.stake_of(id) > 0)
                        .unwrap_or(false);
                    if !staked {
                        return false;
                    }
                }
                // Sybil stake-floor gate (anti-cheat): when `[sybil].min_stake`
                // (whole TON) is set, a candidate must clear that bonded stake to
                // qualify — raising the cost of minting throwaway identities.
                // Default `0` ⇒ off; like the staked-hosts gate it fails closed
                // when no registry/stake is present.
                if cfg.sybil.min_stake > 0 {
                    const TON: Amount = 1_000_000_000;
                    let need = cfg.sybil.min_stake as Amount * TON;
                    let staked = self
                        .stake_registry
                        .as_ref()
                        .map(|r| r.stake_of(id))
                        .unwrap_or(0);
                    if staked < need {
                        return false;
                    }
                }
                true
            }
            // Unknown id (TOFU) can't be tracked → keep, UNLESS a stake gate is on
            // (an unverifiable peer cannot be proven bonded → drop, fail closed).
            None => !cfg.scheduler.require_staked_hosts && cfg.sybil.min_stake == 0,
            }
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
            // Hand the estimator's scanned-bytes estimate to workers so they can
            // size their metered `estimated_seconds`/`cap_seconds` bid.
            cost_hint_bytes: estimated_input_bytes,
            data_class,
            nonce: rand::thread_rng().gen(),
            // Stamp the request-scoping constraints (§7.5) so each host enforces
            // them at admission (the always-correct check): the target partition,
            // the requester's group claims, and the accepted regions.
            network: scope.network.clone(),
            groups: scope.groups.clone(),
            regions: scope.regions.clone(),
            group_proof: scope.group_proof.clone(),
            // Hint the pinned input fingerprint so a worker can early-decline if
            // it already knows it cannot read that exact snapshot.
            input_fingerprint_hint: pinned_fingerprint.clone(),
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
                let reply = tokio::time::timeout(offer_timeout, send_offer(&conn, &offer))
                    .await
                    .ok()?
                    .ok()?;
                Some((conn, reply))
            }
        });

        for (conn, bid) in futures_util::future::join_all(offer_futures)
            .await
            .into_iter()
            .flatten()
        {
            let worker = conn.peer_node_id().clone();
            if let BidDecision::Accept = bid.decision {
                conns.insert(worker.clone(), conn);
                accepted.push((worker, bid));
            }
        }

        // 3. Filter by attestation gate + min trust, score, select top-k.
        let now = now_ts();
        let ab = &cfg.antiabuse;
        let auto_block = ab.enabled
            && ab.blocklist.auto_block_enabled
            && ab.blocklist.auto_block_trust_floor > 0.0;
        // Anti-spoof: gate on the VERIFIED attestation level (a `> L0` self-report
        // is only honored with valid evidence bound to this offer's nonce), not on
        // the bid's raw self-reported integer.
        let offer_nonce = offer.nonce.to_le_bytes();
        // 3a. HARD FLOORS (unchanged): attestation gate, region-attested tier, and
        //     the `min_trust` floor on the EFFECTIVE TRUST score, plus the auto-block
        //     side effect. Selection-score prioritization (3b) only ever reorders
        //     the candidates that already clear these floors.
        let eligible: Vec<(NodeId, Bid, f64)> = accepted
            .into_iter()
            .filter(|(_, bid)| {
                attestation_gate(
                    self.verified_attestation_level(&bid.attestation, &offer_nonce),
                    min_level,
                )
            })
            // Region attested tier: when the requester pins regions AND trusts
            // regions only via attestation, a bidder must present a `region_proof`
            // that verifies against the configured region issuer (bound to its id)
            // for one of the accepted regions; otherwise it is excluded (fail
            // closed). The declared default and no-region queries are unaffected.
            .filter(|(worker, bid)| {
                if cfg.membership.region_trust != p2p_config::RegionTrust::Attested
                    || scope.regions.is_empty()
                {
                    return true;
                }
                self.region_proof_ok(worker, bid, &scope.regions, &cfg.membership.region_issuers)
            })
            // Size-based capability gate (Part B), free-memory half: drop a bidder
            // whose advertised free memory (extended by any PROVEN sustained peak)
            // cannot hold this job's estimated peak working set. Cold-start safe —
            // a bidder advertising unknown (`0`) free memory is never excluded here.
            // The no-estimate path skips the (locked) capability lookup entirely.
            .filter(|(worker, bid)| {
                let peak = estimated_peak_bytes.unwrap_or(0);
                peak == 0
                    || advertised_capacity_admits(
                        bid.free_mem_bytes,
                        self.discovery.proven_capacity(worker).as_ref(),
                        peak,
                        cfg.planner.spill_tolerance_bytes,
                    )
            })
            // Capacity-aware RE-ROUTE floor (Part A.1): after a consensus OOM, only
            // route to STRICTLY higher-capacity nodes than the largest that already
            // failed. `capacity_floor == 0` (the steady state) ⇒ no-op; a bidder
            // advertising unknown (`0`) free memory is not excluded (don't gate on
            // what we don't know).
            .filter(|(_, bid)| {
                capacity_floor == 0 || bid.free_mem_bytes == 0 || bid.free_mem_bytes > capacity_floor
            })
            .filter_map(|(worker, bid)| {
                let trust = self.effective_trust(&worker, cfg, now, paid);
                if auto_block && trust < ab.blocklist.auto_block_trust_floor {
                    if let Some(bl) = &self.blocklist {
                        bl.block(
                            worker.as_str(),
                            p2p_config::BlockKind::NodeId,
                            "auto-block: trust below floor",
                            "auto",
                        );
                    }
                }
                if trust < cfg.trust.min_trust {
                    return None;
                }
                Some((worker, bid, trust))
            })
            .collect();

        // Retain each eligible worker's bid for the per-worker metered cap deadline
        // and the winner's metered settle terms.
        let bid_by_node: HashMap<NodeId, Bid> =
            eligible.iter().map(|(w, b, _)| (w.clone(), b.clone())).collect();

        // 3b. COMPOSITE SELECTION SCORE (performance prioritization): blend the
        //     counterparty-MEASURED perf (size-normalized latency + throughput from
        //     `perf_sample` — receipt-driven, anti-game), the effective trust /
        //     quality, and stake, traded off against the normalized bid PRICE.
        //     Wires economics.ranking.{w_quality,w_price,w_stake,w_latency,
        //     w_throughput}. A faster/cheaper honest node ranks higher; a node that
        //     over-claims speed (low ETA) but is MEASURED slow ranks lower because
        //     its perf_sample reflects reality, not its self-reported ETA. Cold-start
        //     newcomers use a neutral perf prior + the exploration bonus already
        //     folded into the effective-trust term.
        let (pmin, pmax) = eligible
            .iter()
            .fold((u64::MAX, 0u64), |(lo, hi), (_, b, _)| {
                let p = bid_price_signal(b);
                (lo.min(p), hi.max(p))
            });
        // Capacity routing hint (NEVER a trust input): when `capability_weight`
        // is opted in (>0, default 0), break ties toward peers whose gossiped,
        // signed self-capability profile proves a larger handled result size. At
        // the default weight the hint is 0 for everyone ⇒ byte-identical ordering.
        let cap_weight = cfg.economics.ranking.capability_weight;
        let cap_hint = |id: &NodeId| -> u64 {
            if cap_weight > 0.0 {
                // Size-appropriate capability tie-break (Part B): bias toward the
                // peer with the larger PROVEN input size it has handled, not the
                // result-row count — input bytes are what a size gate cares about.
                self.discovery
                    .proven_capacity(id)
                    .map(|p| p.max_input_bytes)
                    .unwrap_or(0)
            } else {
                0
            }
        };
        let mut scored: Vec<(NodeId, f64, u64)> = eligible
            .iter()
            .map(|(worker, bid, trust)| {
                let price_score = normalize_price(bid_price_signal(bid), pmin, pmax);
                let comp = self.selection_score(worker, *trust, price_score, cfg, paid);
                (worker.clone(), comp, bid.eta_ms)
            })
            .collect();
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.2.cmp(&b.2))
                .then_with(|| cap_hint(&b.0).cmp(&cap_hint(&a.0)))
        });
        scored.truncate(cfg.scheduler.replicas);

        if scored.len() < cfg.scheduler.quorum {
            return Ok(AttemptResult::InsufficientWorkers {
                have: scored.len(),
                quorum: cfg.scheduler.quorum,
            });
        }

        // 3c. UP-FRONT escrow coverage preflight (time-based pricing): before any
        //     dispatch, size the escrow to the WORST-CASE `cap_base` among the
        //     selected metered bids and REJECT the job now if the requester's
        //     maxEscrow can't cover `cap_base + φ + κ·N`. This forces enough money
        //     up front (rather than failing mid-settle). Inert unless this is a PAID
        //     job, the selected bids carry metered terms, AND a maxEscrow ceiling
        //     (`pricing.max_bid`) is configured.
        if paid && cfg.economics.enabled && cfg.economics.pricing.max_bid > 0 {
            let bytes = estimated_input_bytes.unwrap_or(0);
            let worst_cap_base = scored
                .iter()
                .filter_map(|(w, _, _)| bid_by_node.get(w))
                .filter_map(MeteredTerms::from_bid)
                .map(|t| t.cap_base(bytes))
                .max()
                .unwrap_or(0);
            if worst_cap_base > 0 {
                let n_verifiers = scored.len().saturating_sub(1);
                let fee_bps = to_bps(cfg.economics.fees.platform_fee_pct) as u16;
                let comm_bps =
                    to_bps(cfg.economics.fees.participation_commission_frac) as u16;
                let max_escrow =
                    (cfg.economics.pricing.max_bid as Amount).saturating_mul(TON_NANOTON);
                if let Err(e) = ensure_escrow_covers(
                    max_escrow,
                    worst_cap_base,
                    n_verifiers,
                    fee_bps,
                    comm_bps,
                ) {
                    return Err(CoordinatorError::InsufficientEscrow(e.to_string()));
                }
            }
        }

        // 4. Dispatch to selected workers and collect commits — progress-stall
        //    aware: a streamed Progress resets the stall timer; no progress (nor a
        //    Commit) within the stall window / attempt deadline ⇒ that provider is
        //    treated as silent (job-fault, no penalty) and the job re-dispatched.
        let verify_mode = match cfg.scheduler.verify_mode {
            VerifyModeCfg::Fast => VerifyMode::Fast,
            VerifyModeCfg::Quorum => VerifyMode::Quorum,
        };
        // Per-job storage access (architecture §9.2). Two mutually-exclusive ways
        // to grant a worker read access to the job's pinned objects, chosen by
        // `credential_mode`:
        //
        //  * PRESIGNED (default, open commodity grid): the REQUESTER signs a
        //    short-TTL read URL per pinned object and ships them in `signed_inputs`.
        //    The worker rewrites the SQL's object refs and reads via plain HTTPS
        //    with NO `CREATE SECRET` — zero reusable secret reaches the host. Only
        //    possible when the inputs are pinned (we need the concrete object URIs).
        //
        //  * SCOPED_SECRET / SEALED: mint a short-lived, read-only credential
        //    scoped to the resolved input prefix and attach it so the worker builds
        //    a `CREATE SECRET`. Scoped to exactly the pinned objects' common prefix
        //    (falls back to the provider root only when unpinned). Delivered
        //    UNSEALED here (sealing needs the winner's attestation-bound key, which
        //    L0 hosts don't ship — see the sealing gap).
        //
        // Unwired providers (default, no remote access) ⇒ neither is attached: the
        // unchanged local-data path.
        let mut credential = None;
        let mut signed_inputs: Vec<p2p_proto::SignedInput> = Vec::new();
        match self.credential_mode {
            p2p_config::CredentialMode::Presigned => {
                if let (Some(p), Some(snap)) =
                    (self.presign_provider.as_ref(), input_snapshot.as_ref())
                {
                    for obj in &snap.objects {
                        match p.presign(&obj.uri, cfg.storage.credential_ttl_secs) {
                            Ok(url) => signed_inputs.push(p2p_proto::SignedInput {
                                uri: obj.uri.clone(),
                                url,
                            }),
                            Err(e) => {
                                tracing::warn!(uri = %obj.uri, "presign failed: {e}");
                            }
                        }
                    }
                }
            }
            p2p_config::CredentialMode::ScopedSecret | p2p_config::CredentialMode::Sealed => {
                credential = self.credential_provider.as_ref().map(|p| {
                    let prefix = input_snapshot
                        .as_ref()
                        .map(credential_prefix)
                        .unwrap_or_default();
                    p.issue(&prefix, cfg.storage.credential_ttl_secs)
                });
            }
        }
        let dispatch = Dispatch {
            job_id: job_id.clone(),
            sql: sql.to_string(),
            query_hash: query_hash.clone(),
            credential,
            memory_limit_bytes: cfg.budget.per_job_memory_bytes,
            threads: cfg.budget.per_job_threads,
            verify_mode,
            sealed_key: None,
            result_parallelism: Some(cfg.transport.result.parallelism as u32),
            compression: Some(crate::compression::algo_to_wire(
                cfg.transport.compression.algorithm,
            )),
            // The pinned manifest the workers must read (None ⇒ unpinned).
            input_snapshot: input_snapshot.clone(),
            // Presigned per-object read URLs (empty ⇒ the secret-based path).
            signed_inputs,
            // Per-worker hard cap deadline is stamped in the collect loop below.
            cap_deadline_ms: None,
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
            let mut dispatch = dispatch.clone();
            // Per-worker HARD cap deadline from ITS OWN bid's `cap_seconds` (metered
            // model): the worker aborts exactly at the cap it promised. `None` ⇒ a
            // fixed/free bid: the worker's own `job_timeout` governs (today).
            let cap_ms = bid_by_node
                .get(worker)
                .and_then(MeteredTerms::from_bid)
                .map(|t| t.cap_seconds.saturating_mul(1000));
            dispatch.cap_deadline_ms = cap_ms;
            // Wait at least the cap (+ stall slack) for a metered worker to commit
            // OR report its cap-abort; otherwise the configured attempt deadline.
            let this_attempt_deadline = match cap_ms {
                Some(ms) => attempt_deadline
                    .max(Duration::from_millis(ms.saturating_add(stall_ms))),
                None => attempt_deadline,
            };
            let worker = worker.clone();
            let progress = self.progress.as_ref();
            async move {
                collect_one(
                    conn,
                    &dispatch,
                    worker,
                    stall_to,
                    this_attempt_deadline,
                    progress,
                )
                .await
            }
        });

        let mut inflight: Vec<InFlight> = Vec::new();
        // (node, classified verdict, RAW failure detail). The detail (e.g. the
        // real DuckDB "Binder Error: …" text) is retained so a consensus failure
        // can surface the actual underlying cause to the requester, not just the
        // generic classification.
        let mut failed: Vec<(NodeId, Verdict, String)> = Vec::new();
        let mut silent: Vec<NodeId> = Vec::new();
        for c in futures_util::future::join_all(collect_futs).await {
            match c {
                Collected::Committed(f) => inflight.push(f),
                Collected::Failed(node, detail) => {
                    let verdict = classify_failure(&detail);
                    failed.push((node, verdict, detail));
                }
                Collected::Silent(node) => silent.push(node),
            }
        }

        // 5. Quorum decision (or canary judgement), with fault attribution to
        //    decide retry-vs-stop on a no-quorum outcome.
        let canary_expected = self.canary.as_ref().and_then(|c| c.expected(query_hash));

        // 5a. Deterministic-input verification: tally quorum GROUPED BY the input
        //     snapshot each provider read. A minority that read a DIFFERENT
        //     (non-empty) fingerprint than the one we pinned read newer/older
        //     bytes — benign input drift, NOT a fault. Re-pin + re-dispatch
        //     (never penalize them). A split on the SAME fingerprint is still a
        //     genuine equivocation; an empty fingerprint (older worker) is treated
        //     as "on the pinned snapshot" so it can never be a false drift. With
        //     no pin (no resolver / no source) `drifted` is always 0 and this
        //     degrades to plain result-hash quorum (today's behavior).
        let commit_keys: Vec<CommitKey> = inflight
            .iter()
            .map(|f| CommitKey {
                input_fingerprint: f.input_fingerprint.as_str(),
                result_hash: f.hash.as_str(),
            })
            .collect();
        let fp_outcome = canonical::evaluate_quorum_on_commits(
            &commit_keys,
            pinned_fingerprint.as_deref(),
            cfg.scheduler.quorum,
        );
        if canary_expected.is_none() && fp_outcome.drifted > 0 {
            tracing::warn!(
                job = %job_id,
                query = %query_hash.0,
                drifted = fp_outcome.drifted,
                committed = inflight.len(),
                "input drift: provider(s) read a different input snapshot than pinned; \
                 re-pinning a fresh snapshot and re-dispatching (benign, no penalty)"
            );
            // Neutral receipts only (no penalty, no commitment failure): the data
            // changed under the providers — that is not their fault.
            self.emit_failure_receipts(&inflight, job_id, query_hash, cfg);
            return Ok(AttemptResult::InputDrift {
                reason: format!(
                    "{} of {} committed provider(s) read a different input snapshot than pinned \
                     (source data changed between executions)",
                    fp_outcome.drifted,
                    inflight.len()
                ),
            });
        }
        let outcome = fp_outcome.pinned;
        let non_verifiable = inputs_not_pinnable
            || (cfg.antiabuse.nondeterminism_active()
                && canary_expected.is_none()
                && is_nondeterministic(sql));

        // Tally how the selected providers fared (for consensus-infeasible).
        let mut tally = FaultTally::new(selected.len());
        for _ in &inflight {
            tally.record_committed();
        }
        for (_, v, _) in &failed {
            tally.record(*v);
        }
        for _ in &silent {
            tally.record_silent();
        }
        let frac = cfg.antiabuse.fault_attribution.job_consensus_fraction;

        // Providers to ROUTE AROUND on a re-dispatch: only those that failed or
        // went silent. A provider that COMMITTED a (correct) result but merely
        // lost the hedged race is healthy and stays eligible for retries — never
        // excluded (do NOT pass the whole `selected` set). (Part A.2.)
        let failed_or_silent: Vec<NodeId> = failed
            .iter()
            .map(|(n, _, _)| n.clone())
            .chain(silent.iter().cloned())
            .collect();
        // The OOMed providers (consensus resource-exceeded re-route) and the
        // largest free-memory capacity that already failed — so the loop only
        // keeps re-routing while a STRICTLY higher-capacity node could exist.
        let oomed: Vec<NodeId> = failed
            .iter()
            .filter(|(_, v, _)| matches!(v, Verdict::ResourceExceeded))
            .map(|(n, _, _)| n.clone())
            .collect();
        let max_tried_capacity = oomed
            .iter()
            .map(|n| bid_by_node.get(n).map(|b| b.free_mem_bytes).unwrap_or(0))
            .max()
            .unwrap_or(0);

        let (agreed_hash, verified) = match (&canary_expected, verify_mode) {
            (Some(expected), _) => (Some(expected.clone()), true),
            _ if non_verifiable => {
                if inflight.is_empty() {
                    return Ok(AttemptResult::Inconclusive {
                        tried: failed_or_silent.clone(),
                    });
                }
                let fastest = inflight
                    .iter()
                    .min_by_key(|f| f.latency_ms)
                    .map(|f| f.hash.clone());
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
                    // Consensus-INFEASIBLE query (bad SQL: syntax/binder/catalog) →
                    // a TERMINAL job fault. Neutral receipts, no penalty, refund,
                    // and STOP re-dispatching — retrying cannot help. Surface the
                    // REAL underlying error (e.g. the DuckDB "Binder Error: …" text)
                    // the providers reported, not just the generic classification.
                    let mut reason = tally
                        .consensus_reason(frac)
                        .unwrap_or_else(|| "consensus-infeasible query".to_string());
                    if let Some(detail) = consensus_failure_detail(&failed) {
                        reason = format!("{reason}: {detail}");
                    }
                    self.emit_failure_receipts(&inflight, job_id, query_hash, cfg);
                    return Ok(AttemptResult::Infeasible { reason });
                } else if cfg.antiabuse.fault_attribution_active()
                    && tally.is_consensus_resource_exceeded(frac)
                {
                    // Consensus RESOURCE-EXCEEDED (OOM / too big) → NOT terminal.
                    // Exclude the OOMed nodes and re-route to higher-capacity nodes
                    // (the loop only continues while a strictly bigger node could
                    // exist; otherwise it surfaces `ExceedsCapacity`). No provider
                    // penalty (job fault, not a provider fault).
                    let mut reason = tally
                        .resource_exceeded_reason(frac)
                        .unwrap_or_else(|| "consensus resource-exceeded query".to_string());
                    if let Some(detail) = consensus_failure_detail(&failed) {
                        reason = format!("{reason}: {detail}");
                    }
                    self.emit_failure_receipts(&inflight, job_id, query_hash, cfg);
                    return Ok(AttemptResult::ResourceExceeded {
                        oomed,
                        max_tried_capacity,
                        reason,
                    });
                } else if inflight.len() >= cfg.scheduler.quorum {
                    // Enough providers committed but their hashes disagree — a
                    // genuine verification disagreement (terminal).
                    self.emit_failure_receipts(&inflight, job_id, query_hash, cfg);
                    return Ok(AttemptResult::QuorumDisagreement {
                        agreement: outcome.agreement,
                        quorum: outcome.quorum,
                    });
                } else {
                    // Shortfall from silence / transient failures → re-dispatch
                    // (route only around the non-delivering providers).
                    return Ok(AttemptResult::Inconclusive {
                        tried: failed_or_silent.clone(),
                    });
                }
            }
            (None, VerifyMode::Fast) => {
                if inflight.is_empty() {
                    return Ok(AttemptResult::Inconclusive {
                        tried: failed_or_silent.clone(),
                    });
                }
                let fastest = inflight
                    .iter()
                    .min_by_key(|f| f.latency_ms)
                    .map(|f| f.hash.clone());
                (fastest, outcome.reached())
            }
        };

        let agreed = match agreed_hash {
            Some(h) => h,
            None => {
                return Ok(AttemptResult::Inconclusive {
                    tried: failed_or_silent.clone(),
                });
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
                return Ok(AttemptResult::Inconclusive {
                    tried: failed_or_silent.clone(),
                });
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

        // Requester-observed commit latencies of the AGREEING quorum (winner + every
        // matching non-winner) — the metering truth + its anti-overcharge cross-check
        // basis. The winner's latency is the billing latency; the median of the set
        // caps it (× tolerance) so a single slow/over-reporting winner can't over-bill.
        let winner_latency_ms = winner.latency_ms;
        let mut agreeing_latencies: Vec<u64> = vec![winner_latency_ms];
        if !non_verifiable {
            for f in losers.iter() {
                if f.hash == agreed {
                    agreeing_latencies.push(f.latency_ms);
                }
            }
        }

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
        //
        // The requester MEASURES the winner's delivered result (rows + serialized
        // bytes) — a counterparty-attested capability signal that the provider
        // cannot inflate. Losers are reset before transfer, so only the winner
        // carries a measured magnitude.
        //
        // `input_bytes` is the pre-flight estimator's scanned-bytes ESTIMATE for
        // this job (exact scanned bytes aren't available requester-side). It is
        // attested as the workload magnitude feeding the per-job perf aggregate +
        // proven-capability size; `0` keeps the "fully unknown" semantics when no
        // estimate was threaded through. We do NOT fabricate a value.
        let winner_obs = ObservedSize {
            input_bytes: estimated_input_bytes.unwrap_or(0),
            result_rows: result.as_ref().map(|r| r.row_count() as u64).unwrap_or(0),
            result_bytes: result
                .as_ref()
                .and_then(|r| p2p_proto::to_bytes(r).ok())
                .map(|b| b.len() as u64)
                .unwrap_or(0),
        };
        for f in std::iter::once(&winner).chain(losers.iter()) {
            let is_winner = std::ptr::eq(f, &winner);
            if !is_winner && f.hash == agreed && !non_verifiable {
                agreeing_non_winners.push(f.worker.clone());
            }
            let verdict = if non_verifiable {
                Verdict::Inconclusive
            } else if is_winner && result.is_none() {
                // The winner committed the agreed hash but its result transfer
                // failed/timed out (7b): the attempt is abandoned as inconclusive
                // and re-dispatched. Do NOT book a `Correct` success it never
                // delivered — and no penalty either (an at-transfer silence is a
                // job-fault here, indistinguishable from a transport hiccup).
                Verdict::Inconclusive
            } else if f.hash == agreed {
                Verdict::Correct
            } else if pinned_fingerprint
                .as_deref()
                .map_or(true, |p| f.input_fingerprint == p)
            {
                // Wrong result on PROVABLY the same inputs (the provider reported
                // the pinned fingerprint, or the job is unpinned) — a genuine
                // fault (deterministic-input verification).
                Verdict::Incorrect
            } else {
                // Wrong result, but the provider did NOT prove it read the pinned
                // snapshot (an empty/unknown fingerprint on a pinned job — e.g. an
                // older worker that did not echo one). Non-attributable: never a
                // false slash. Any provider on a DIFFERENT non-empty fingerprint
                // was already routed to the benign InputDrift re-dispatch above.
                Verdict::Inconclusive
            };

            // A provider that committed a NON-matching hash on a verified-feasible
            // job broke its commitment (distinct from being merely slow).
            if verified && matches!(verdict, Verdict::Incorrect) {
                commitment_failers.push(f.worker.clone());
            }

            let obs = if is_winner {
                winner_obs
            } else {
                ObservedSize::default()
            };
            receipts.push(self.make_receipt(
                job_id,
                &f.worker,
                query_hash,
                &f.hash,
                verdict,
                f.latency_ms,
                obs,
                &f.input_fingerprint,
            ));
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
                receipts.push(self.make_receipt(
                    job_id,
                    node,
                    query_hash,
                    "",
                    Verdict::Timeout,
                    0,
                    ObservedSize::default(),
                    pinned_fingerprint.as_deref().unwrap_or(""),
                ));
                let penalty = penalty_for(true);
                if penalty > 0.0 {
                    self.trust_store.penalize(node, penalty);
                }
                commitment_failers.push(node.clone());
            }
            for (node, verdict, _) in &failed {
                // Use the classified verdict (neutral for an "at capacity" decline,
                // resource/infeasible for a job fault). Only an actual provider
                // fault penalizes / counts as a broken commitment.
                receipts.push(self.make_receipt(
                    job_id,
                    node,
                    query_hash,
                    "",
                    *verdict,
                    0,
                    ObservedSize::default(),
                    pinned_fingerprint.as_deref().unwrap_or(""),
                ));
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
            // Fold the requester-measured workload magnitude into the peer's
            // proven-capability aggregate (no-op for non-Correct receipts).
            self.trust_store.observe_capability(r);
            // Fold the measured latency + workload bytes into the peer's rolling
            // perf aggregate (EWMA, time-decayed). CAPTURE-only — exposed via
            // `perf_sample` for a later prioritization worker; it does NOT change
            // selection scoring here (no-op for non-Correct receipts).
            self.trust_store.observe_perf(r);
        }

        let result = match result {
            Some(r) => r,
            None => {
                // Winner did not actually stream a result — re-dispatch. The
                // requester observation is booked only on a terminal outcome (as
                // in every other inconclusive re-dispatch path), never for an
                // attempt that produced no usable result.
                return Ok(AttemptResult::Inconclusive {
                    tried: failed_or_silent.clone(),
                });
            }
        };

        self.trust_store.record_requester(&self_id, verified, now);

        // Resolve the WINNER's time-based pricing terms (if its bid was metered):
        // bill `base = rate × min(billed_seconds, cap_seconds) (+ byte term)`, where
        // `billed_seconds` is the requester-observed commit latency cross-checked
        // against the quorum verifiers' median. `None` ⇒ the fixed-price path.
        let metered = bid_by_node
            .get(&winner_id)
            .and_then(MeteredTerms::from_bid)
            .map(|terms| {
                let billed_seconds = metered_billed_seconds(
                    winner_latency_ms,
                    &agreeing_latencies,
                    cfg.economics.pricing.metering_tolerance,
                );
                MeteredSettle {
                    terms,
                    billed_seconds,
                    bytes: estimated_input_bytes.unwrap_or(0),
                }
            });

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
            pinned_fingerprint.as_deref().unwrap_or(""),
            metered,
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
        // Carry the estimator's scanned-bytes + peak working-set estimates into
        // the per-job observed input size and the size-based capability gate.
        let estimated_input_bytes = estimate.as_ref().map(|e| e.scanned_uncompressed_bytes);
        let estimated_peak_bytes = estimate.as_ref().map(|e| e.peak_working_set_bytes);
        // The free local path runs the node's own LOCKED-DOWN engine
        // (`enable_external_access=false`, `disabled_filesystems`), which cannot
        // read external data. A data-source query therefore can never run locally
        // in `auto` — it must go to the grid (exactly today's behavior). So only
        // hand the planner an estimate for pure in-memory queries; the size
        // estimate still drives the REMOTE capability gate + bid sizing below. An
        // explicit `prefer => 'local'` is still honored (the local-decision path is
        // unchanged for forced modes — `try_local_execution` consults the planner).
        let local_estimate = if crate::estimator::has_data_source(sql) {
            None
        } else {
            estimate
        };
        if let Some(outcome) = self.try_local_execution(sql, &cfg, local_estimate).await? {
            return Ok(outcome);
        }
        // Planner chose remote (or failed over): dispatch to the grid, threading
        // the estimate through for the receipt's observed input size and the
        // size-based capability gate.
        self.dispatch_to_grid(sql, cfg, &overrides, estimated_input_bytes, estimated_peak_bytes)
            .await
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
        // failed reservation means the local budget / shared governor is full (or
        // we lost the slot race) → grid. The governed executor floors a
        // no-/tiny-estimate job to a representative per-job footprint so own
        // queries are accounted against the process-wide capacity cap (see
        // `LocalExecutor::governed`).
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
        // Collateral-wallet aggregation (G4): when a binding source is wired, the
        // reputation + observation count are summed across every node id bound to
        // the same wallet, so a key rotation can't shed history. Off ⇒ per-node.
        let (reputation, obs) = self.wallet_reputation(worker, cfg, now);
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
        // Measured proven-capability term (grid-wide capability model): bias
        // selection toward peers COUNTERPARTY-measured to handle real work. The
        // weight defaults to 0.0, so this is a strict no-op (byte-identical
        // selection) until an operator opts in. An inflated self-claim earns
        // nothing here because the term is driven purely by measured successes.
        let capability_weight = cfg.economics.ranking.capability_weight;
        let cap_term = if capability_weight > 0.0 {
            capability_weight
                * self.trust_store.capability_confidence(
                    worker,
                    rep.prior_alpha,
                    rep.prior_beta,
                    rep.confidence_z,
                )
        } else {
            0.0
        };
        let score = (soft_trust_score(&cfg.trust.weights, &inputs) + bonus + cap_term).clamp(0.0, 1.0);
        // Newcomer trust ceiling (anti-cheat): a thin-history node cannot exceed
        // `newcomer_trust_ceiling` until it has `newcomer_obs_threshold` verified
        // observations — so a freshly minted identity (even a well-staked or
        // vouched one) can't jump straight into the top ranks. Default ceiling
        // `1.0` ⇒ no-op (byte-identical selection).
        let ranking = &cfg.economics.ranking;
        if ranking.newcomer_trust_ceiling < 1.0 && obs < ranking.newcomer_obs_threshold {
            score.min(ranking.newcomer_trust_ceiling)
        } else {
            score
        }
    }

    /// Expose a worker's rolling per-job performance as a
    /// [`p2p_settlement::QualitySample`] for a later prioritization / pricing
    /// worker to consume. Built from the trust store's time-decayed perf
    /// aggregate (EWMA latency + workload bytes) plus the worker's recency-weighted
    /// success ratio. Returns `None` when no perf has been observed.
    ///
    /// CAPTURE / EXPOSE only: this does NOT change selection scoring (that stays
    /// receipt/`capability_confidence`-driven). The pricing/prioritization worker
    /// is the component that will later act on this sample.
    pub fn perf_sample(&self, worker: &NodeId) -> Option<p2p_settlement::QualitySample> {
        let perf = self.trust_store.perf_aggregate(worker)?;
        let now = now_ts();
        // Confidence-aware success ratio if any reputation history exists, else a
        // neutral 1.0 (perf alone says nothing about correctness).
        let success_ratio = self.trust_store.reputation(worker, now).unwrap_or(1.0);
        Some(p2p_settlement::QualitySample::new(
            success_ratio,
            perf.ewma_latency_ms.round().max(0.0) as u64,
            perf.ewma_bytes.round().max(0.0) as u64,
        ))
    }

    /// The economics-gated stake factor for selection: the diminishing/capped
    /// `stake_factor` ONLY for a PAID job with economics enabled and a registry
    /// wired (otherwise `0.0` — today's behavior, no chain nudge). Mirrors the
    /// gate in [`Coordinator::effective_trust`].
    fn stake_factor_for(&self, worker: &NodeId, cfg: &GridConfig, paid: bool) -> f64 {
        if paid && cfg.economics.enabled {
            self.stake_registry
                .as_ref()
                .map(|r| r.stake_factor(worker))
                .unwrap_or(0.0)
        } else {
            0.0
        }
    }

    /// Composite selection score (performance prioritization): a weight-normalized
    /// blend of the COUNTERPARTY-MEASURED perf (size-normalized latency `L` +
    /// throughput `T` from [`Coordinator::perf_sample`] — receipt-driven, anti-game),
    /// the effective trust/quality, and stake, traded off against the normalized bid
    /// `price_score` (cheaper ⇒ higher). Wires `economics.ranking.{w_quality,
    /// w_latency,w_throughput,w_stake,w_price}`.
    ///
    /// A faster/higher-throughput honest node ranks higher; a node that over-claims
    /// speed (low self-reported ETA) but is MEASURED slow ranks lower, because `L`/`T`
    /// come from `perf_sample` not from the bid. A cold-start node with no perf
    /// history uses a NEUTRAL `0.5` prior for `L`/`T` (its exploration bonus +
    /// newcomer ceiling are already folded into the `effective_trust` term). When all
    /// weights are `0` the score collapses to the effective trust (today's ranking).
    ///
    /// The stake term is **reliability-GATED** ([`blend_selection_score`] /
    /// [`stake_reliability_factor`]): the raw `stake_factor` is scaled by the node's
    /// STAKE-INDEPENDENT confidence-aware reputation against
    /// `economics.ranking.stake_reliability_floor`, so a higher stake only amplifies
    /// the ranking of already-reliable nodes and earns ~nothing for low-reputation
    /// ones — it can never rescue a bad node nor (combined with the unchanged hard
    /// `min_trust`/attestation floors) lower the first-try verified-success rate.
    fn selection_score(
        &self,
        worker: &NodeId,
        trust: f64,
        price_score: f64,
        cfg: &GridConfig,
        paid: bool,
    ) -> f64 {
        let q = &cfg.economics.quality;
        let r = &cfg.economics.ranking;
        // Counterparty-measured perf → size-normalized latency + throughput scores.
        // No history ⇒ neutral 0.5 priors (cold-start newcomers are sampled via the
        // exploration bonus already in `trust`, not via a fabricated perf score).
        let (lat, thr) = match self.perf_sample(worker) {
            Some(s) => (
                latency_score(s.latency_ms, s.bytes_verified, q),
                throughput_score(s.latency_ms, s.bytes_verified, q),
            ),
            None => (0.5, 0.5),
        };
        let stake = self.stake_factor_for(worker, cfg, paid);
        // Reliability gate (success-rate guardrail): the stake term is scaled by
        // the node's STAKE-INDEPENDENT confidence-aware reputation (verified-success
        // rate), NOT by the stake-inclusive `trust`, so extra stake can never
        // inflate its own gate nor lift a low-reputation node above a reliable one.
        // Skip the lookup entirely when there is no stake (free/chain-off path).
        let reliability = if stake > 0.0 {
            self.wallet_reputation(worker, cfg, now_ts()).0
        } else {
            0.0
        };
        blend_selection_score(trust, lat, thr, stake, reliability, price_score, r)
    }

    #[allow(clippy::too_many_arguments)]
    fn make_receipt(
        &self,
        job_id: &JobId,
        worker: &NodeId,
        query_hash: &QueryHash,
        result_hash: &str,
        verdict: Verdict,
        latency_ms: u64,
        obs: ObservedSize,
        input_fingerprint: &str,
    ) -> Receipt {
        let draft = ReceiptDraft {
            job_id: job_id.clone(),
            worker_id: worker.clone(),
            query_hash: query_hash.clone(),
            result_hash: result_hash.to_string(),
            verdict,
            latency_ms,
            ts: now_ts(),
            observed_input_bytes: obs.input_bytes,
            observed_result_rows: obs.result_rows,
            observed_result_bytes: obs.result_bytes,
            input_fingerprint: input_fingerprint.to_string(),
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
            let r = self.make_receipt(
                job_id,
                &f.worker,
                query_hash,
                &f.hash,
                verdict,
                f.latency_ms,
                ObservedSize::default(),
                &f.input_fingerprint,
            );
            self.trust_store.record(&r);
        }
        debug!("emitted failure receipts for job {job_id}");
    }

    /// Resolve a worker's on-chain ADDRESS (used for the committed candidate set
    /// and the settle `winner` field). EVERY node has a stable address here —
    /// including free nodes (their derived address) — so candidate-set membership
    /// and the winner field are always well-formed. A free node simply never
    /// receives funds (its winner/commission leg is 0). Resolution order: the
    /// wallet binding's bound address (if any) → the legacy `wallet_resolver` →
    /// the deterministic BLAKE3 derivation.
    fn wallet_of(&self, node: &NodeId) -> WalletAddress {
        if let Some(b) = &self.wallet_binding {
            if let Some(w) = b(node) {
                return w;
            }
        }
        match &self.wallet_resolver {
            Some(r) => r(node),
            None => WalletAddress::new(0, *blake3::hash(node.as_str().as_bytes()).as_bytes()),
        }
    }

    /// FREE-NODE POLICY: the worker's PAYOUT wallet, or `None` if it is a free
    /// (walletless) node that cannot be paid. When a `wallet_binding` is wired it is
    /// authoritative (`None` ⇒ free node). Otherwise every node is payable (the
    /// `wallet_resolver`/derivation), preserving today's all-wallet behavior. Used
    /// to decide the winner payout (`base` vs `0`) and which verifiers earn κ.
    fn payout_wallet_of(&self, node: &NodeId) -> Option<WalletAddress> {
        if let Some(b) = &self.wallet_binding {
            return b(node);
        }
        Some(self.wallet_of(node))
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
        input_fingerprint: &str,
        metered: Option<MeteredSettle>,
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
        // Time-based (usage) pricing sizes the escrow `B` to the WORST-CASE
        // `cap_base` so the metered `base = rate × min(actual, cap)` always fits and
        // the unused remainder refunds to the requester. The FIXED path (no metered
        // terms) keeps today's behavior: `B = max_bid` and a B-derived base.
        let fee_bps_pre = to_bps(cfg.economics.fees.platform_fee_pct);
        let comm_bps_pre = to_bps(cfg.economics.fees.participation_commission_frac);
        let max_bid = match &metered {
            Some(m) => {
                let cap_base = m.terms.cap_base(m.bytes);
                let n = agreeing_non_winners.len();
                required_escrow_total(cap_base, n, fee_bps_pre as u16, comm_bps_pre as u16)
            }
            None => escrow_bid_nanoton(cfg),
        };

        // FEE-RECIPIENT enforcement: the platform-fee recipient (treasury) is the
        // AUTHORITATIVE on-chain `GlobalParams.fee_recipient` for the pinned
        // `version`, NOT local config. Honest-party rejection: if a local
        // `economics.fee_recipient` is configured AND disagrees with the chain
        // value, refuse to open/settle (the admin treasury must not be silently
        // overridden). Precedence: chain wins; local must match (else reject). When
        // the chain value is unknown (no params source synced), we proceed on the
        // configured treasury (documented residual assumption) — paid/on-chain nodes
        // wire a params source so the chain recipient is always known.
        let chain_fee_recipient = self.current_fee_recipient();
        if let Some(chain) = chain_fee_recipient {
            if let Some(local_str) = cfg
                .economics
                .fee_recipient
                .as_deref()
                .filter(|s| !s.trim().is_empty())
            {
                match WalletAddress::from_any_str(local_str) {
                    Ok(local) if local != chain => {
                        debug!(
                            "refusing to open escrow: local economics.fee_recipient {} != on-chain GlobalParams.fee_recipient {} (params_version {version}) — admin treasury cannot be overridden",
                            local.to_raw_string(),
                            chain.to_raw_string(),
                        );
                        return;
                    }
                    Ok(_) => {}
                    Err(_) => {
                        debug!("refusing to open escrow: local economics.fee_recipient is unparseable; chain GlobalParams.fee_recipient is authoritative");
                        return;
                    }
                }
            }
        }

        // Open the per-job escrow binding the lock + params version into its terms
        // (hence its deterministic address). The mock/noop rails fall back to the
        // termless `open_escrow`; the ton rail builds the on-chain terms cell. On
        // the live rail this call now BLOCKS until the funded deploy is on-chain
        // confirmed (compute+action succeeded), and `settle` below likewise waits
        // for its confirmation — so we never settle against an unfunded escrow nor
        // report a job paid on a settle that actually failed/aborted.
        //
        // The requester-committed payout-eligible candidate set: the winner plus
        // every agreeing non-winner (their payout wallets). The `ton` rail binds
        // `candidatesCommitment(set)` into the per-job escrow terms at open and the
        // settle below re-presents the SAME payees, so the on-chain candidate-set
        // check passes. The mock/noop rails ignore it.
        let mut candidate_wallets: Vec<WalletAddress> =
            Vec::with_capacity(1 + agreeing_non_winners.len());
        candidate_wallets.push(self.wallet_of(winner));
        for n in agreeing_non_winners {
            let w = self.wallet_of(n);
            if !candidate_wallets.contains(&w) {
                candidate_wallets.push(w);
            }
        }
        let handle = match settlement.open_escrow_with_terms(
            job_id,
            max_bid,
            &result_hash,
            version,
            &candidate_wallets,
            // Bind the chain-authoritative fee recipient (admin treasury) into the
            // per-job terms; the `ton` rail rejects a mismatching local treasury.
            chain_fee_recipient,
        ) {
            Ok(h) => h,
            Err(e) => {
                debug!("open_escrow failed: {e}");
                return;
            }
        };

        let b = handle.max_bid;
        // φ / κ as INTEGER basis points — identical to the value bound into
        // `EscrowTerms.platformFeeBps` and to the on-chain settle enforcement
        // (`platformFee == winnerAmount*φ/10000`), so the produced split passes the
        // strict on-chain fee-equality check byte-for-byte.
        let fee_bps = to_bps(cfg.economics.fees.platform_fee_pct);
        let comm_bps = to_bps(cfg.economics.fees.participation_commission_frac);
        let n = agreeing_non_winners.len() as Amount;
        // The winner's settled price `base`:
        //  * METERED (time-based): `base = rate × min(billed_seconds, cap_seconds)
        //    (+ byte term)` — the ACTUAL metered cost. Since B was sized to the
        //    worst-case `cap_base ≥ base`, the full split fits B and the unused
        //    remainder (B − total) refunds to the requester (over-reservation
        //    minus actual). On-chain checks stay valid: `base` IS the metered cost.
        //  * FIXED (today): derive `base` from B so the FULL multi-party split
        //    (winner base + φ·base + κ·base×N) spends ~all of B (winner takes the
        //    integer-floor remainder); `total ≤ B` always holds.
        let base = match &metered {
            Some(m) => m.terms.base_for(m.billed_seconds, m.bytes).min(b),
            None => {
                let denom = BPS_DENOM
                    .saturating_add(fee_bps)
                    .saturating_add(n.saturating_mul(comm_bps));
                b.saturating_mul(BPS_DENOM) / denom
            }
        };
        let fee = base.saturating_mul(fee_bps) / BPS_DENOM;
        let commission_each = base.saturating_mul(comm_bps) / BPS_DENOM;
        // FREE-NODE POLICY: the platform fee is φ·base on EVERY paid job regardless
        // of the node mix. Only WALLET-holding agreeing verifiers earn the κ
        // commission (free verifiers fully participate but earn nothing); their
        // payout key ∈ the committed candidate set. The denominator above used the
        // full agreeing count, so dropping free verifiers only shrinks the spend
        // (more refunds to the requester) — the split still fits B.
        let participants: Vec<Payout> = agreeing_non_winners
            .iter()
            .filter(|node| self.payout_wallet_of(node).is_some())
            .map(|node| Payout {
                to: self.wallet_of(node),
                amount: commission_each,
            })
            .collect();
        // The winner is paid EXACTLY its quoted price `base` when it is a WALLET
        // node; a FREE (walletless) winner is paid `0` and the base is left in the
        // escrow to refund to the requester (B3 carry-all). `base` remains the fee
        // base either way, so the platform still collects φ·base.
        let winner_has_wallet = self.payout_wallet_of(winner).is_some();
        let winner_amount = if winner_has_wallet { base } else { 0 };
        // Up-front coverage guard (the off-chain twin of the on-chain `total ≤ B`
        // bound): require B to cover the worst case base + φ·base + κ·base×(paid
        // verifiers), so the base can always be paid (wallet winner) or refunded
        // (free winner) and every party gets their correct share. Reject rather than
        // under-pay; the derived base fits, so this passes in steady state and
        // catches a misconfiguration with a clear, human-readable error.
        if let Err(e) = ensure_escrow_covers(
            b,
            base,
            participants.len(),
            fee_bps as u16,
            comm_bps as u16,
        ) {
            debug!("escrow coverage preflight failed, not settling: {e}");
            return;
        }
        let outcome = SettlementOutcome {
            result_hash,
            base,
            winner: Payout {
                to: self.wallet_of(winner),
                amount: winner_amount,
            },
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
                // Bind the input snapshot the verified result was computed over.
                input_fingerprint: input_fingerprint.to_string(),
            });
        }
    }
}

/// Derive a read-only credential SCOPE prefix covering every pinned object in
/// `snapshot` (deterministic-input verification). Uses the longest common path
/// prefix of the object URIs, trimmed to the last `/`, so the issued credential
/// is scoped to this job's input directory instead of the whole provider root.
/// Empty (provider root) only when there are no objects.
fn credential_prefix(snapshot: &InputSnapshot) -> String {
    let mut uris = snapshot.objects.iter().map(|o| o.uri.as_str());
    let Some(first) = uris.next() else {
        return String::new();
    };
    let mut prefix = first.to_string();
    for u in uris {
        let common: String = prefix
            .chars()
            .zip(u.chars())
            .take_while(|(x, y)| x == y)
            .map(|(x, _)| x)
            .collect();
        prefix = common;
    }
    match prefix.rfind('/') {
        Some(i) => prefix[..=i].to_string(),
        None => prefix,
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

/// Convert a `[0,1]` fraction to integer basis points (rounded, clamped to u16
/// range). This is the SAME conversion `GlobalParams::from_config_parts` and the
/// settlement wiring use, so the escrow's bound `platformFeeBps`, the on-chain
/// fee-equality enforcement, and the coordinator's split all agree exactly.
fn to_bps(frac: f64) -> Amount {
    (frac * 10_000.0).round().clamp(0.0, 65_535.0) as Amount
}

/// Pick a representative RAW failure detail from a consensus failure (preferring
/// an `Infeasible` verdict, the deterministic class), cleaned of the internal
/// wrapper prefixes so the requester sees the core engine message (e.g.
/// `Binder Error: …`) instead of `exec error: execution failed: query: …`.
fn consensus_failure_detail(failed: &[(NodeId, Verdict, String)]) -> Option<String> {
    let raw = failed
        .iter()
        .find(|(_, v, _)| matches!(v, Verdict::Infeasible))
        .or_else(|| failed.first())
        .map(|(_, _, d)| d.as_str())?;
    let cleaned = clean_engine_detail(raw);
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

/// Strip the internal error-wrapper prefixes (`exec error:` → `execution failed:`
/// → `query:`/`prepare:`/…) that accrete as an engine error travels worker →
/// requester, leaving the underlying engine message.
fn clean_engine_detail(detail: &str) -> String {
    const PREFIXES: &[&str] = &[
        "exec error: ",
        "execution failed: ",
        "query rejected by lockdown policy: ",
        "query: ",
        "prepare: ",
        "fetch: ",
    ];
    let mut s = detail.trim();
    loop {
        let mut stripped = false;
        for p in PREFIXES {
            if let Some(rest) = s.strip_prefix(p) {
                s = rest.trim_start();
                stripped = true;
                break;
            }
        }
        if !stripped {
            break;
        }
    }
    s.to_string()
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

/// Map the config data class onto the proto wire enum carried in an `Offer`.
fn data_class_from_cfg(c: DataClassCfg) -> DataClass {
    match c {
        DataClassCfg::Public => DataClass::Public,
        DataClassCfg::Internal => DataClass::Internal,
        DataClassCfg::Sensitive => DataClass::Sensitive,
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
        if write_msg(&mut send, &Wire::Dispatch(dispatch.clone()))
            .await
            .is_err()
        {
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
                        input_fingerprint: c.input_fingerprint,
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

#[cfg(test)]
mod capacity_gate_tests {
    use super::*;

    fn profile(max_input: u64, max_temp: u64, max_peak: u64, successes: u64) -> CapabilityProfile {
        CapabilityProfile {
            schema_version: 1,
            node_id: NodeId("b3:w".into()),
            pubkey: "00".repeat(32),
            max_input_bytes: max_input,
            max_result_rows: 0,
            max_result_bytes: 0,
            max_peak_memory_bytes: max_peak,
            max_temp_dir_bytes: max_temp,
            successes,
            seq: 1,
            ts: 0,
            sig: String::new(),
        }
    }

    #[test]
    fn proven_gate_is_cold_start_safe() {
        // No estimate ⇒ always admit (route as today).
        assert!(proven_capacity_admits(None, 0));
        assert!(proven_capacity_admits(Some(&profile(10, 0, 0, 100)), 0));
        // Newcomer (no proven profile) is NEVER excluded, even by a huge job.
        assert!(proven_capacity_admits(None, u64::MAX));
        // Thin proven history (< PROVEN_GATE_MIN_SUCCESSES) is not trusted as a
        // hard ceiling ⇒ not excluded, even when the job exceeds its tiny max.
        assert!(proven_capacity_admits(
            Some(&profile(1_000, 0, 0, PROVEN_GATE_MIN_SUCCESSES - 1)),
            1_000_000_000
        ));
    }

    #[test]
    fn proven_gate_excludes_only_a_confident_over_ceiling_node() {
        // Confident node (enough successes): a job within proven max + spill is
        // admitted; one clearly beyond it is excluded.
        let p = profile(1_000_000, 500_000, 0, PROVEN_GATE_MIN_SUCCESSES);
        assert!(proven_capacity_admits(Some(&p), 1_400_000)); // within max+temp
        assert!(!proven_capacity_admits(Some(&p), 5_000_000)); // clearly beyond
    }

    #[test]
    fn advertised_gate_is_cold_start_safe_and_spill_aware() {
        let spill = 512 * 1024 * 1024;
        // No estimate ⇒ admit; unknown (0) free_mem ⇒ admit (don't gate on the
        // unknown), even with a huge peak.
        assert!(advertised_capacity_admits(0, None, 0, spill));
        assert!(advertised_capacity_admits(0, None, u64::MAX, spill));
        // Fits within free_mem + spill ⇒ admit; clearly beyond ⇒ exclude.
        let free = 1024 * 1024; // 1 MiB advertised
        assert!(advertised_capacity_admits(free, None, free + spill, spill));
        assert!(!advertised_capacity_admits(free, None, free + spill + 1, spill));
        // A PROVEN sustained peak (+ proven spill) extends the effective capacity,
        // so a node that has really run this big before is admitted.
        let p = profile(0, 256 * 1024 * 1024, 4 * 1024 * 1024 * 1024, 10);
        assert!(advertised_capacity_admits(free, Some(&p), 4 * 1024 * 1024 * 1024, spill));
    }
}

#[cfg(test)]
mod metering_tests {
    use super::*;

    fn bid_metered(rate: u64, rate_gb: u64, cap_seconds: u64) -> Bid {
        Bid {
            job_id: JobId::new(),
            worker_id: NodeId("b3:w".into()),
            decision: BidDecision::Accept,
            eta_ms: 10,
            price: 0,
            attestation: Attestation::stub_l0(),
            recent_receipts: vec![],
            free_mem_bytes: 0,
            free_threads: 0,
            region_proof: None,
            rate_per_second: rate,
            rate_per_gb: rate_gb,
            estimated_seconds: cap_seconds / 5,
            cap_seconds,
        }
    }

    #[test]
    fn metered_terms_only_present_with_a_rate() {
        // A fixed/free bid (rate 0) carries no metered terms ⇒ the fixed path.
        let mut fixed = bid_metered(0, 0, 0);
        fixed.estimated_seconds = 0;
        assert!(MeteredTerms::from_bid(&fixed).is_none());
        // A metered bid (rate > 0) yields terms.
        assert!(MeteredTerms::from_bid(&bid_metered(1000, 0, 50)).is_some());
    }

    #[test]
    fn base_is_rate_times_actual_capped_at_cap_seconds() {
        // rate = 1000 nanoton/s, cap = 50 s (no byte term).
        let t = MeteredTerms::from_bid(&bid_metered(1000, 0, 50)).unwrap();
        // Bill the actual seconds when under the cap.
        assert_eq!(t.base_for(10, 0), 10_000);
        assert_eq!(t.base_for(50, 0), 50_000);
        // Past the cap, billing is CLAMPED to cap_seconds (the billing ceiling).
        assert_eq!(t.base_for(9_999, 0), 50_000);
        // cap_base is the worst case = rate × cap_seconds.
        assert_eq!(t.cap_base(0), 50_000);
        // The settled base for any actual ≤ cap_base, so the remainder refunds.
        assert!(t.base_for(10, 0) < t.cap_base(0));
    }

    #[test]
    fn optional_per_gb_byte_term_adds_to_base() {
        // rate/s = 0-effect here (cap_seconds 1), rate/GiB = 2000 nanoton/GiB.
        let t = MeteredTerms::from_bid(&bid_metered(1, 2000, 1)).unwrap();
        // 2 GiB scanned ⇒ + 2 × 2000 byte term.
        let two_gib = 2 * GIB_BYTES as u64;
        assert_eq!(t.base_for(1, two_gib), 1 /* rate×1s */ + 4000);
        // Sub-GiB floors to 0 byte term (integer GiB granularity).
        assert_eq!(t.base_for(1, 100), 1);
    }

    #[test]
    fn billed_seconds_cross_check_caps_an_overreporting_winner() {
        // Winner reports 8s; the quorum verifiers measured ~2s. With tolerance 1.5
        // the billed seconds are capped at ceil(2000ms × 1.5)=ceil(3s)=3 — the
        // winner cannot over-bill against the measured median.
        let billed = metered_billed_seconds(8_000, &[8_000, 2_000, 2_000], 1.5);
        assert_eq!(billed, 3, "over-reporting winner capped at median × tolerance");
        // When the winner is in line with the median, its own (smaller) seconds win.
        let billed = metered_billed_seconds(2_000, &[2_000, 2_100, 1_900], 1.5);
        assert_eq!(billed, 2);
        // ceil(ms/1000) semantics: 1ms ⇒ 1 second; 0ms ⇒ 0.
        assert_eq!(ceil_secs(1), 1);
        assert_eq!(ceil_secs(0), 0);
        assert_eq!(ceil_secs(1001), 2);
    }

    #[test]
    fn median_and_price_normalization() {
        assert_eq!(median_ms(&[]), 0);
        assert_eq!(median_ms(&[5]), 5);
        assert_eq!(median_ms(&[1, 3]), 2);
        assert_eq!(median_ms(&[5, 1, 3]), 3);
        // Cheaper ⇒ higher score; equal ⇒ neutral 1.0.
        assert_eq!(normalize_price(10, 10, 10), 1.0);
        assert_eq!(normalize_price(10, 10, 20), 1.0); // min price ⇒ best
        assert_eq!(normalize_price(20, 10, 20), 0.0); // max price ⇒ worst
        assert!((normalize_price(15, 10, 20) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn full_metered_split_fits_escrow_and_refunds_the_remainder() {
        // rate = 1 TON/s, cap = 5000 s ⇒ cap_base = 5000 TON. Bill 2 actual seconds
        // ⇒ base = 2 TON. φ = 15%, κ = 5%, N = 1 verifier.
        const TON: Amount = 1_000_000_000;
        let t = MeteredTerms::from_bid(&bid_metered(TON as u64, 0, 5000)).unwrap();
        let cap_base = t.cap_base(0);
        let b = required_escrow_total(cap_base, 1, 1500, 500); // sized to cap_base
        let base = t.base_for(2, 0);
        let fee = base * 1500 / 10_000;
        let comm = base * 500 / 10_000;
        let total = base + fee + comm;
        assert_eq!(base, 2 * TON);
        assert_eq!(fee, base * 15 / 100);
        assert_eq!(comm, base * 5 / 100);
        assert!(total <= b, "metered split must fit the cap-sized escrow");
        // The unused over-reservation (cap minus actual) refunds to the requester.
        assert!(b - total > 0, "unused escrow refunds (cap >> actual)");
    }
}

/// Selection-score stake weighting + the reliability gate that protects the
/// first-try verified-success rate. These exercise the pure scoring math
/// (`blend_selection_score` / `stake_reliability_factor`) plus the real
/// `soft_trust_score` hard-floor function, so both properties are proven without
/// standing up a full networked coordinator.
#[cfg(test)]
mod selection_scoring_tests {
    use super::*;
    use p2p_config::{GridConfig, RankingEconomics};
    use p2p_trust::{soft_trust_score, TrustInputs};

    /// The reliability gate: zero at/below the floor, linear ramp to 1 at full
    /// reliability, and a plain pass-through (no dead-zone) at `floor == 0`.
    #[test]
    fn reliability_gate_zeroes_below_floor_and_ramps_above() {
        let floor = 0.5;
        // At/below the floor stake earns NO credit.
        assert_eq!(stake_reliability_factor(0.0, floor), 0.0);
        assert_eq!(stake_reliability_factor(0.5, floor), 0.0);
        assert_eq!(stake_reliability_factor(0.2, floor), 0.0);
        // Above the floor it ramps linearly to 1.0 at perfect reliability.
        assert!((stake_reliability_factor(0.75, floor) - 0.5).abs() < 1e-9);
        assert!((stake_reliability_factor(1.0, floor) - 1.0).abs() < 1e-9);
        // floor == 0 ⇒ the factor is exactly the reliability (still scaled, no
        // dead-zone).
        assert!((stake_reliability_factor(0.3, 0.0) - 0.3).abs() < 1e-9);
        // Monotonic non-decreasing in reliability.
        assert!(stake_reliability_factor(0.9, floor) > stake_reliability_factor(0.7, floor));
    }

    /// PROPERTY 1 — stake matters MORE now. Between two equally-reliable nodes
    /// (same trust/perf/price), the higher-staked one ranks clearly above the
    /// lower-staked one, AND the ranking gap is strictly larger under the new
    /// doubled `w_stake` than it was under the previous 0.15 weight.
    #[test]
    fn higher_stake_ranks_higher_and_more_than_before() {
        let r_new = RankingEconomics::default(); // w_stake = 0.30
        let mut r_old = RankingEconomics::default();
        r_old.w_stake = 0.15; // the previous default

        // Equal, comfortably-reliable nodes; only the stake differs.
        let (trust, lat, thr, rel, price) = (0.8, 0.5, 0.5, 0.9, 1.0);
        let (stake_lo, stake_hi) = (0.2, 0.8);

        let hi_new = blend_selection_score(trust, lat, thr, stake_hi, rel, price, &r_new);
        let lo_new = blend_selection_score(trust, lat, thr, stake_lo, rel, price, &r_new);
        assert!(hi_new > lo_new, "more stake must rank higher (new weight)");

        let hi_old = blend_selection_score(trust, lat, thr, stake_hi, rel, price, &r_old);
        let lo_old = blend_selection_score(trust, lat, thr, stake_lo, rel, price, &r_old);

        let gap_new = hi_new - lo_new;
        let gap_old = hi_old - lo_old;
        assert!(
            gap_new > gap_old,
            "doubling w_stake must widen stake's pull: gap_new={gap_new} gap_old={gap_old}"
        );
    }

    /// PROPERTY 2 — the success rate is protected. A HIGH-stake but LOW-reputation
    /// node does NOT outrank a reliable, low-stake node, because the reliability
    /// gate zeroes its stake credit. The counterfactual (crediting its stake in
    /// full, as a naive additive term would) shows it WOULD have won — proving the
    /// gate is exactly what prevents the regression.
    #[test]
    fn high_stake_low_reputation_cannot_outrank_a_reliable_node() {
        let r = RankingEconomics::default(); // floor 0.5, w_stake 0.30

        // Reliable node A: strong reputation/trust, only a little stake.
        let (trust_a, rel_a, stake_a) = (0.85, 0.9, 0.1);
        // Unreliable node B: poor reputation/trust, MAX stake.
        let (trust_b, rel_b, stake_b) = (0.45, 0.2, 1.0);
        // Identical neutral perf + price so stake/reliability decide the order.
        let (lat, thr, price) = (0.5, 0.5, 1.0);

        let score_a = blend_selection_score(trust_a, lat, thr, stake_a, rel_a, price, &r);
        let score_b = blend_selection_score(trust_b, lat, thr, stake_b, rel_b, price, &r);
        assert!(
            score_a > score_b,
            "a max-stake low-reputation node must NOT outrank a reliable one: A={score_a} B={score_b}"
        );
        // B is below the reliability floor ⇒ its stake earns literally nothing.
        assert_eq!(stake_reliability_factor(rel_b, r.stake_reliability_floor), 0.0);

        // Counterfactual: had B's stake been credited in full (reliability forced
        // to 1.0, i.e. an UN-gated additive stake term), B would have outranked A.
        let score_b_ungated = blend_selection_score(trust_b, lat, thr, stake_b, 1.0, price, &r);
        assert!(
            score_b_ungated > score_a,
            "without the gate, raw stake WOULD flip the order (B={score_b_ungated} > A={score_a}) — the gate is what protects success rate"
        );
    }

    /// PROPERTY 2 (hard floor) — stake cannot buy past `min_trust`. The eligibility
    /// gate runs on the stake-inclusive effective trust BEFORE scoring; using the
    /// real `soft_trust_score` with default weights, a zero-reputation node with
    /// the MAXIMUM stake factor (and max age/voucher) still scores below the
    /// default `min_trust`, so it is excluded from candidacy entirely.
    #[test]
    fn max_stake_zero_reputation_stays_below_min_trust() {
        let cfg = GridConfig::default();
        let weights = &cfg.trust.weights;

        // Everything stacked in the staker's favor EXCEPT a verified track record.
        let inputs = TrustInputs {
            reputation: 0.0,
            age_factor: 1.0,
            voucher_trust: 1.0,
            stake_factor: 1.0,
            penalties: 0.0,
        };
        let trust = soft_trust_score(weights, &inputs);
        assert!(
            trust < cfg.trust.min_trust,
            "stake/vouchers/age must not buy past min_trust without reputation: trust={trust} min_trust={}",
            cfg.trust.min_trust
        );

        // A node with a genuine verified history clears the same floor.
        let reliable = TrustInputs {
            reputation: 1.0,
            age_factor: 1.0,
            voucher_trust: 0.0,
            stake_factor: 0.0,
            penalties: 0.0,
        };
        assert!(soft_trust_score(weights, &reliable) >= cfg.trust.min_trust);
    }
}
