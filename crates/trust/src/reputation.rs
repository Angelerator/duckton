//! Reputation & trust scoring (architecture §7.3 / §7.5).
//!
//! Reputation is a **recency-weighted correctness rate** built from verified
//! receipts:  `R = Σ wᵢ·correctᵢ / Σ wᵢ`,  `wᵢ = decay^age · job_weight`.
//!
//! The store is **pluggable** behind [`TrustStore`] so tests use the in-memory
//! implementation while production can use a persistent embedded store. All
//! caches are **bounded** (per [`p2p_config::LimitsConfig`]) — no unbounded maps.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use p2p_config::{LimitsConfig, ReputationWeights, TrustConfig};
use p2p_proto::{AttestationLevel, NodeId, Receipt};

/// Current unix-seconds timestamp.
pub fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A stable fingerprint of a receipt's *identity* — `(job_id, requester_id)`. A
/// requester issues exactly one receipt per job, so within a given worker's
/// history this uniquely identifies a receipt; it deliberately excludes the
/// verdict/latency so an equivocating requester can't create two observations
/// for the same job. Used to dedup replayed/re-presented receipts.
pub(crate) fn receipt_fingerprint(r: &Receipt) -> u64 {
    let mut h = blake3::Hasher::new();
    h.update(b"duckdb-p2p-receipt-fp-v1");
    h.update(r.job_id.0.as_bytes());
    h.update(&[0]);
    h.update(r.requester_id.0.as_bytes());
    let bytes = h.finalize();
    let mut x = [0u8; 8];
    x.copy_from_slice(&bytes.as_bytes()[..8]);
    u64::from_le_bytes(x)
}

/// One recorded outcome for a worker.
#[derive(Debug, Clone, Copy)]
struct Observation {
    ts: u64,
    correct: bool,
    weight: f64,
    /// Fingerprint of the source receipt (for replay dedup + eviction cleanup).
    fp: u64,
}

/// Bounded, O(1) measured-capability aggregate for one peer (the grid-wide
/// proven-power signal). Built from counterparty-attested receipts: a provider
/// cannot inflate it because the magnitudes are MEASURED by the requester, not
/// claimed by the provider. Maxima ratchet up on verified success; `successes`
/// feeds a confidence shrink so a peer seen once at a size is not yet trusted to
/// do it routinely.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ProvenCapability {
    /// Largest input the peer returned a verified answer for (bytes; `0` = none).
    pub max_input_bytes: u64,
    /// Largest result the requester actually received from the peer (rows).
    pub max_result_rows: u64,
    /// Largest result the requester actually received from the peer (bytes).
    pub max_result_bytes: u64,
    /// Count of verified successful observations backing the maxima.
    pub successes: u32,
    /// Timestamp of the most recent successful observation.
    pub last_ts: u64,
}

/// Bounded, O(1) **per-worker performance aggregate** (architecture: per-job
/// perf measurement). A time-decayed EWMA of measured latency / workload bytes /
/// throughput from each verified-success receipt, so a later prioritization /
/// pricing worker can rank by recent observed performance.
///
/// This is a CAPTURE-and-EXPOSE signal only: it is built from the same
/// requester-MEASURED magnitudes as `ProvenCapability` and MUST NOT (yet) feed
/// selection scoring — that stays receipt/`capability_confidence`-driven.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PerfAggregate {
    /// Time-decayed EWMA of observed latency (ms).
    pub ewma_latency_ms: f64,
    /// Time-decayed EWMA of observed workload bytes (estimated scanned input).
    pub ewma_bytes: f64,
    /// Time-decayed EWMA of observed throughput (bytes/second).
    pub ewma_throughput_bps: f64,
    /// Number of verified-success samples folded in.
    pub obs_count: u64,
    /// Timestamp (unix-seconds) of the most recent folded sample.
    pub last_ts: u64,
}

/// Minimum weight a fresh sample gets in the time-decayed EWMA, so back-to-back
/// (same-second) samples still move the aggregate rather than being ignored when
/// the elapsed-time decay is ~1.0.
const PERF_MIN_ALPHA: f64 = 0.1;

/// Fold one sample into a time-decayed EWMA. The new sample's weight grows with
/// the time elapsed since the last sample (older accumulator ⇒ staler ⇒ trust the
/// new value more), floored at [`PERF_MIN_ALPHA`]. `half_life_secs <= 0` disables
/// decay (every sample gets full weight beyond the floor's complement).
pub(crate) fn ewma_fold(prev: f64, sample: f64, dt_secs: f64, half_life_secs: f64) -> f64 {
    let decay = if half_life_secs > 0.0 {
        0.5f64.powf(dt_secs.max(0.0) / half_life_secs)
    } else {
        0.0
    };
    let alpha = (1.0 - decay).max(PERF_MIN_ALPHA);
    prev * (1.0 - alpha) + sample * alpha
}

/// Update a [`PerfAggregate`] in place with a verified-success sample.
pub(crate) fn perf_fold(
    agg: &mut PerfAggregate,
    latency_ms: u64,
    bytes: u64,
    ts: u64,
    half_life_secs: f64,
) {
    let lat = latency_ms as f64;
    let by = bytes as f64;
    // Throughput in bytes/sec from this single observation (`0` latency ⇒ treat
    // as 1ms so a sub-millisecond job doesn't divide by zero).
    let tput = by / (latency_ms.max(1) as f64 / 1000.0);
    if agg.obs_count == 0 {
        agg.ewma_latency_ms = lat;
        agg.ewma_bytes = by;
        agg.ewma_throughput_bps = tput;
    } else {
        let dt = ts.saturating_sub(agg.last_ts) as f64;
        agg.ewma_latency_ms = ewma_fold(agg.ewma_latency_ms, lat, dt, half_life_secs);
        agg.ewma_bytes = ewma_fold(agg.ewma_bytes, by, dt, half_life_secs);
        agg.ewma_throughput_bps = ewma_fold(agg.ewma_throughput_bps, tput, dt, half_life_secs);
    }
    agg.obs_count = agg.obs_count.saturating_add(1);
    agg.last_ts = agg.last_ts.max(ts);
}

/// Pluggable reputation/receipt store.
///
/// Implementations must be cheap to share (`Arc<dyn TrustStore>`); methods take
/// `&self` and handle their own interior synchronization.
pub trait TrustStore: Send + Sync {
    /// Ingest a verified receipt (caller must have checked the signature).
    fn record(&self, receipt: &Receipt);

    /// Ingest a verified receipt's MEASURED workload magnitude into the peer's
    /// proven-capability aggregate (architecture: grid-wide measured-capability
    /// model). Only `Correct` receipts contribute, deduped by the same
    /// `(job, requester)` fingerprint as [`Self::record`] so replays cannot
    /// inflate it. Default no-op so existing stores/tests compile unchanged.
    fn observe_capability(&self, _receipt: &Receipt) {}

    /// The peer's measured proven capability, or `None` if nothing was ever
    /// observed. Default `None` (stores that don't track capability).
    fn proven_capability(&self, _worker: &NodeId) -> Option<ProvenCapability> {
        None
    }

    /// Fold a verified-success receipt's MEASURED latency + workload bytes into
    /// the peer's rolling [`PerfAggregate`] (EWMA, time-decayed). Only `Correct`
    /// receipts contribute, deduped by the same `(job, requester)` fingerprint as
    /// [`Self::observe_capability`] so replays cannot skew it. Default no-op so
    /// existing stores/tests compile unchanged. CAPTURE-only — see
    /// [`PerfAggregate`]; it does NOT feed selection scoring.
    fn observe_perf(&self, _receipt: &Receipt) {}

    /// The peer's rolling performance aggregate, or `None` if nothing was ever
    /// observed. Default `None`.
    fn perf_aggregate(&self, _worker: &NodeId) -> Option<PerfAggregate> {
        None
    }

    /// A `[0,1]` confidence in the peer's proven capability: the Wilson-shrunk
    /// fraction of measured successes, so a peer with thin capability history is
    /// not yet treated as fully proven. `0.0` when no capability is recorded.
    /// Reuses [`confidence_reputation`] over the success count. Default `0.0`.
    fn capability_confidence(
        &self,
        worker: &NodeId,
        prior_alpha: f64,
        prior_beta: f64,
        z: f64,
    ) -> f64 {
        match self.proven_capability(worker) {
            Some(cap) if cap.successes > 0 => {
                confidence_reputation(1.0, cap.successes as usize, prior_alpha, prior_beta, z)
            }
            _ => 0.0,
        }
    }
    /// Recency-weighted correctness rate in `[0,1]` for a worker (`now` = clock).
    fn reputation(&self, worker: &NodeId, now: u64) -> Option<f64>;
    /// Number of recorded observations for a worker.
    fn observation_count(&self, worker: &NodeId) -> usize;
    /// **Confidence-aware** reputation (BLOCKCHAIN_ECONOMICS §4.1/§7.3): the raw
    /// recency-weighted success ratio shrunk toward a pessimistic prior via the
    /// Wilson lower bound, so a node with few verified jobs is not treated as
    /// fully trusted. Returns `None` exactly when [`Self::reputation`] does (no
    /// history). The default impl derives it from the raw ratio + observation
    /// count, so no store needs to override it.
    fn confident_reputation(
        &self,
        worker: &NodeId,
        now: u64,
        prior_alpha: f64,
        prior_beta: f64,
        z: f64,
    ) -> Option<f64> {
        let ratio = self.reputation(worker, now)?;
        let obs = self.observation_count(worker);
        Some(confidence_reputation(
            ratio,
            obs,
            prior_alpha,
            prior_beta,
            z,
        ))
    }
    /// Add voucher trust to a worker (Phase 3 web-of-trust).
    fn add_vouch(&self, worker: &NodeId, weight: f64);
    /// Total voucher trust accrued for a worker.
    fn voucher_trust(&self, worker: &NodeId) -> f64;
    /// Apply a penalty (e.g. failed canary).
    fn penalize(&self, worker: &NodeId, amount: f64);
    /// Accumulated penalty for a worker.
    fn penalty(&self, worker: &NodeId) -> f64;
    /// Number of distinct workers currently tracked.
    fn tracked_workers(&self) -> usize;

    // --- Requester reputation / age (ARCHITECTURE "Abuse resistance") --------
    // Requesters get a reputation + age too, so a job's effect on a provider's
    // score can be weighted by the requester's standing ("newer sender → less
    // effect"). Default impls are no-ops/zero so existing stores keep compiling.

    /// Record that a requester completed a job (`correct` = the job produced a
    /// usable, verified outcome). Builds the requester's age + reputation.
    fn record_requester(&self, _requester: &NodeId, _correct: bool, _ts: u64) {}
    /// Number of recorded observations for a requester (its "age").
    fn requester_observation_count(&self, _requester: &NodeId) -> usize {
        0
    }
    /// Recency-weighted success rate in `[0,1]` for a requester (or `None` if
    /// the requester has no history).
    fn requester_reputation(&self, _requester: &NodeId, _now: u64) -> Option<f64> {
        None
    }
}

struct WorkerState {
    observations: VecDeque<Observation>,
    /// Fingerprints of receipts already counted, for replay dedup. Kept in lock
    /// step with `observations` (pruned on FIFO eviction) so it stays bounded.
    seen: HashSet<u64>,
    voucher_trust: f64,
    penalty: f64,
    /// Measured proven-capability aggregate (O(1) maxima + success count).
    capability: ProvenCapability,
    /// Fingerprints of receipts already folded into `capability` (replay dedup),
    /// with a FIFO order so the set stays bounded by the observation cap.
    cap_seen: HashSet<u64>,
    cap_seen_order: VecDeque<u64>,
    /// Rolling per-job performance aggregate (EWMA, time-decayed).
    perf: PerfAggregate,
    /// Fingerprints already folded into `perf` (replay dedup), bounded FIFO.
    perf_seen: HashSet<u64>,
    perf_seen_order: VecDeque<u64>,
}

impl WorkerState {
    fn new() -> Self {
        Self {
            observations: VecDeque::new(),
            seen: HashSet::new(),
            voucher_trust: 0.0,
            penalty: 0.0,
            capability: ProvenCapability::default(),
            cap_seen: HashSet::new(),
            cap_seen_order: VecDeque::new(),
            perf: PerfAggregate::default(),
            perf_seen: HashSet::new(),
            perf_seen_order: VecDeque::new(),
        }
    }
}

/// In-memory bounded trust store. Suitable for tests and single-process nodes.
/// Per-worker observation history is capped; the number of tracked workers is
/// capped with FIFO eviction.
pub struct InMemoryTrustStore {
    inner: Mutex<Inner>,
    half_life_secs: f64,
    max_obs_per_worker: usize,
    max_workers: usize,
}

struct Inner {
    workers: HashMap<NodeId, WorkerState>,
    /// Insertion order for FIFO eviction when over `max_workers`.
    order: VecDeque<NodeId>,
    /// Per-requester observation history (bounded, FIFO-evicted like workers).
    requesters: HashMap<NodeId, VecDeque<Observation>>,
    requester_order: VecDeque<NodeId>,
}

impl InMemoryTrustStore {
    pub fn new(trust: &TrustConfig, limits: &LimitsConfig) -> Self {
        Self {
            inner: Mutex::new(Inner {
                workers: HashMap::new(),
                order: VecDeque::new(),
                requesters: HashMap::new(),
                requester_order: VecDeque::new(),
            }),
            half_life_secs: trust.reputation_half_life_secs as f64,
            max_obs_per_worker: limits.receipt_cache_per_worker.max(1),
            max_workers: limits.trust_store_capacity.max(1),
        }
    }

    fn decay_weight(&self, age_secs: f64) -> f64 {
        if self.half_life_secs <= 0.0 {
            1.0
        } else {
            0.5f64.powf(age_secs.max(0.0) / self.half_life_secs)
        }
    }

    fn with_worker<R>(&self, worker: &NodeId, f: impl FnOnce(&mut WorkerState) -> R) -> R {
        let mut inner = self.inner.lock().unwrap();
        if !inner.workers.contains_key(worker) {
            // evict FIFO if at capacity
            while inner.order.len() >= self.max_workers {
                if let Some(evict) = inner.order.pop_front() {
                    inner.workers.remove(&evict);
                } else {
                    break;
                }
            }
            inner.workers.insert(worker.clone(), WorkerState::new());
            inner.order.push_back(worker.clone());
        }
        let state = inner.workers.get_mut(worker).expect("just inserted");
        f(state)
    }
}

impl TrustStore for InMemoryTrustStore {
    fn record(&self, receipt: &Receipt) {
        // Only `Correct` and provable PROVIDER-fault verdicts count against a
        // provider's reputation. Requester/job-caused (`ResourceExceeded` /
        // `Infeasible`) and non-attributable (`Inconclusive`) verdicts are
        // neutral and recorded as nothing — so a heavy/infeasible/non-verifiable
        // job can never be used to grief a provider (fault attribution,
        // ARCHITECTURE "Abuse resistance").
        if !receipt.verdict.affects_reputation() {
            return;
        }
        let correct = receipt.verdict.is_correct();
        let cap = self.max_obs_per_worker;
        let fp = receipt_fingerprint(receipt);
        self.with_worker(&receipt.worker_id, |state| {
            // Replay defense: count each unique (job, requester) receipt at most
            // once, so replaying/re-presenting a captured receipt cannot inflate
            // a provider's reputation.
            if !state.seen.insert(fp) {
                return;
            }
            state.observations.push_back(Observation {
                ts: receipt.ts,
                correct,
                weight: 1.0,
                fp,
            });
            while state.observations.len() > cap {
                if let Some(old) = state.observations.pop_front() {
                    state.seen.remove(&old.fp);
                }
            }
        });
    }

    fn observe_capability(&self, receipt: &Receipt) {
        // Only a verified success demonstrates capability; everything else is
        // neutral (a heavy/infeasible job that the peer legitimately could not do
        // is a job/requester fault, never a capability claim).
        if !receipt.verdict.is_correct() {
            return;
        }
        let cap = self.max_obs_per_worker;
        let fp = receipt_fingerprint(receipt);
        let input = receipt.observed_input_bytes;
        let rows = receipt.observed_result_rows;
        let bytes = receipt.observed_result_bytes;
        let ts = receipt.ts;
        self.with_worker(&receipt.worker_id, |state| {
            // Replay defense: fold each unique (job, requester) receipt once so a
            // replayed receipt cannot inflate the success count.
            if !state.cap_seen.insert(fp) {
                return;
            }
            state.cap_seen_order.push_back(fp);
            while state.cap_seen_order.len() > cap {
                if let Some(old) = state.cap_seen_order.pop_front() {
                    state.cap_seen.remove(&old);
                }
            }
            let c = &mut state.capability;
            c.max_input_bytes = c.max_input_bytes.max(input);
            c.max_result_rows = c.max_result_rows.max(rows);
            c.max_result_bytes = c.max_result_bytes.max(bytes);
            c.successes = c.successes.saturating_add(1);
            c.last_ts = c.last_ts.max(ts);
        });
    }

    fn proven_capability(&self, worker: &NodeId) -> Option<ProvenCapability> {
        let inner = self.inner.lock().unwrap();
        let cap = inner.workers.get(worker)?.capability;
        if cap.successes == 0 {
            None
        } else {
            Some(cap)
        }
    }

    fn observe_perf(&self, receipt: &Receipt) {
        // Only verified successes carry a meaningful latency/throughput sample.
        if !receipt.verdict.is_correct() {
            return;
        }
        let cap = self.max_obs_per_worker;
        let fp = receipt_fingerprint(receipt);
        let latency = receipt.latency_ms;
        let bytes = receipt.observed_input_bytes;
        let ts = receipt.ts;
        let half_life = self.half_life_secs;
        self.with_worker(&receipt.worker_id, |state| {
            // Replay defense: fold each unique (job, requester) receipt once.
            if !state.perf_seen.insert(fp) {
                return;
            }
            state.perf_seen_order.push_back(fp);
            while state.perf_seen_order.len() > cap {
                if let Some(old) = state.perf_seen_order.pop_front() {
                    state.perf_seen.remove(&old);
                }
            }
            perf_fold(&mut state.perf, latency, bytes, ts, half_life);
        });
    }

    fn perf_aggregate(&self, worker: &NodeId) -> Option<PerfAggregate> {
        let inner = self.inner.lock().unwrap();
        let perf = inner.workers.get(worker)?.perf;
        if perf.obs_count == 0 {
            None
        } else {
            Some(perf)
        }
    }

    fn reputation(&self, worker: &NodeId, now: u64) -> Option<f64> {
        let inner = self.inner.lock().unwrap();
        let state = inner.workers.get(worker)?;
        if state.observations.is_empty() {
            return None;
        }
        let mut num = 0.0;
        let mut den = 0.0;
        for o in &state.observations {
            let age = now.saturating_sub(o.ts) as f64;
            let w = o.weight * self.decay_weight(age);
            den += w;
            if o.correct {
                num += w;
            }
        }
        if den == 0.0 {
            None
        } else {
            Some(num / den)
        }
    }

    fn observation_count(&self, worker: &NodeId) -> usize {
        let inner = self.inner.lock().unwrap();
        inner
            .workers
            .get(worker)
            .map(|s| s.observations.len())
            .unwrap_or(0)
    }

    fn add_vouch(&self, worker: &NodeId, weight: f64) {
        self.with_worker(worker, |s| s.voucher_trust += weight);
    }

    fn voucher_trust(&self, worker: &NodeId) -> f64 {
        let inner = self.inner.lock().unwrap();
        inner
            .workers
            .get(worker)
            .map(|s| s.voucher_trust)
            .unwrap_or(0.0)
    }

    fn penalize(&self, worker: &NodeId, amount: f64) {
        self.with_worker(worker, |s| s.penalty += amount);
    }

    fn penalty(&self, worker: &NodeId) -> f64 {
        let inner = self.inner.lock().unwrap();
        inner.workers.get(worker).map(|s| s.penalty).unwrap_or(0.0)
    }

    fn tracked_workers(&self) -> usize {
        self.inner.lock().unwrap().workers.len()
    }

    fn record_requester(&self, requester: &NodeId, correct: bool, ts: u64) {
        let cap = self.max_obs_per_worker;
        let max = self.max_workers;
        let mut inner = self.inner.lock().unwrap();
        if !inner.requesters.contains_key(requester) {
            while inner.requester_order.len() >= max {
                if let Some(evict) = inner.requester_order.pop_front() {
                    inner.requesters.remove(&evict);
                } else {
                    break;
                }
            }
            inner.requesters.insert(requester.clone(), VecDeque::new());
            inner.requester_order.push_back(requester.clone());
        }
        let hist = inner.requesters.get_mut(requester).expect("just inserted");
        // Requester observations are not receipt-deduped (no per-requester `seen`
        // set); `fp` is unused here.
        hist.push_back(Observation {
            ts,
            correct,
            weight: 1.0,
            fp: 0,
        });
        while hist.len() > cap {
            hist.pop_front();
        }
    }

    fn requester_observation_count(&self, requester: &NodeId) -> usize {
        self.inner
            .lock()
            .unwrap()
            .requesters
            .get(requester)
            .map(|h| h.len())
            .unwrap_or(0)
    }

    fn requester_reputation(&self, requester: &NodeId, now: u64) -> Option<f64> {
        let inner = self.inner.lock().unwrap();
        let hist = inner.requesters.get(requester)?;
        if hist.is_empty() {
            return None;
        }
        let mut num = 0.0;
        let mut den = 0.0;
        for o in hist {
            let age = now.saturating_sub(o.ts) as f64;
            let w = o.weight * self.decay_weight(age);
            den += w;
            if o.correct {
                num += w;
            }
        }
        if den == 0.0 {
            None
        } else {
            Some(num / den)
        }
    }
}

/// Hard attestation gate (architecture §7.5): `actual >= required`.
pub fn attestation_gate(actual: AttestationLevel, required: AttestationLevel) -> bool {
    actual >= required
}

/// Inputs to the soft trust score for one worker.
#[derive(Debug, Clone, Copy)]
pub struct TrustInputs {
    /// Recency-weighted reputation `R` in `[0,1]` (or bootstrap value if none).
    pub reputation: f64,
    /// Age/history factor in `[0,1]` (more verified history ⇒ closer to 1).
    pub age_factor: f64,
    /// Voucher trust contribution in `[0,1]`.
    pub voucher_trust: f64,
    /// Stake contribution in `[0,1]`.
    pub stake_factor: f64,
    /// Penalties to subtract (>= 0).
    pub penalties: f64,
}

/// Compute the soft trust score: `clamp(α·R + β·age + γ·voucher + δ·stake − pen, 0, 1)`.
pub fn soft_trust_score(weights: &ReputationWeights, inputs: &TrustInputs) -> f64 {
    let raw = weights.alpha_reputation * inputs.reputation
        + weights.beta_age * inputs.age_factor
        + weights.gamma_voucher * inputs.voucher_trust
        + weights.delta_stake * inputs.stake_factor
        - inputs.penalties;
    raw.clamp(0.0, 1.0)
}

/// Map observation count to an age/history factor in `[0,1]` saturating at
/// `saturate_at` observations.
pub fn age_factor(observations: usize, saturate_at: usize) -> f64 {
    if saturate_at == 0 {
        return 1.0;
    }
    (observations as f64 / saturate_at as f64).min(1.0)
}

/// Confidence-aware reputation (BLOCKCHAIN_ECONOMICS §4.1): the **Wilson
/// lower-confidence-bound** of the success ratio, with configurable Beta
/// pseudo-count priors (`prior_alpha` pseudo-successes, `prior_beta`
/// pseudo-failures). It replaces the raw success ratio so that a node with only
/// a few observations is *not* treated as fully trusted — a "3-for-3" newcomer
/// scores well below a node with a long correct history, defeating cheap
/// reputation farming.
///
/// * `ratio` — the raw (recency-weighted) success ratio in `[0,1]`.
/// * `observations` — number of verified observations backing `ratio`.
/// * `z` — Wilson confidence z-score (e.g. `1.96` ≈ 95% lower bound). `z == 0`
///   collapses to the prior-shrunk Beta posterior **mean** (no interval term).
///
/// Properties (unit-tested): monotonically increasing in `observations` for a
/// fixed `ratio`; approaches `ratio` as `observations → ∞`; strictly below
/// `ratio` for finite `observations` when `ratio` is high.
pub fn confidence_reputation(
    ratio: f64,
    observations: usize,
    prior_alpha: f64,
    prior_beta: f64,
    z: f64,
) -> f64 {
    let n = observations as f64;
    // Fold the Beta pseudo-counts into the observed counts.
    let successes = (ratio.clamp(0.0, 1.0) * n) + prior_alpha.max(0.0);
    let trials = n + prior_alpha.max(0.0) + prior_beta.max(0.0);
    if trials <= 0.0 {
        return 0.0;
    }
    let p = successes / trials;
    if z <= 0.0 {
        // No interval widening: the prior-shrunk posterior mean.
        return p.clamp(0.0, 1.0);
    }
    // Wilson score interval lower bound over (p, trials).
    let z2 = z * z;
    let denom = 1.0 + z2 / trials;
    let centre = p + z2 / (2.0 * trials);
    let margin = z
        * ((p * (1.0 - p) / trials) + z2 / (4.0 * trials * trials))
            .max(0.0)
            .sqrt();
    ((centre - margin) / denom).clamp(0.0, 1.0)
}

/// Cold-start **exploration bonus** (BLOCKCHAIN_ECONOMICS §5.2/§6): an
/// uncertainty term added to a candidate's selection score that decays linearly
/// to zero as the node accrues verified observations. New honest nodes therefore
/// get sampled (and can build reputation) instead of being permanently starved
/// by incumbents. `rate` is the configured exploration rate ε; `saturation` is
/// the observation count at which the bonus reaches zero. Returns `0.0` when
/// `rate == 0` (pure exploitation, today's default behavior).
pub fn exploration_bonus(observations: usize, rate: f64, saturation: usize) -> f64 {
    if rate <= 0.0 || saturation == 0 {
        return 0.0;
    }
    let remaining = 1.0 - (observations as f64 / saturation as f64).min(1.0);
    (rate * remaining).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use p2p_proto::{JobId, QueryHash, Verdict};

    fn store() -> InMemoryTrustStore {
        InMemoryTrustStore::new(&TrustConfig::default(), &LimitsConfig::default())
    }

    fn receipt(worker: &str, verdict: Verdict, ts: u64) -> Receipt {
        Receipt {
            job_id: JobId::new(),
            worker_id: NodeId(worker.into()),
            requester_id: NodeId("b3:req".into()),
            query_hash: QueryHash::compute("SELECT 1", "t"),
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
        }
    }

    #[test]
    fn replayed_receipt_counts_once() {
        // Recording the identical receipt multiple times must yield ONE
        // observation (replay cannot inflate reputation). The `receipt()` helper
        // uses a fresh JobId per call, so we reuse a single instance here.
        let s = store();
        let w = NodeId("b3:w".into());
        let r = receipt("b3:w", Verdict::Correct, 100);
        s.record(&r);
        s.record(&r);
        s.record(&r);
        assert_eq!(s.observation_count(&w), 1);
    }

    #[test]
    fn reputation_is_fraction_correct() {
        let s = store();
        let w = NodeId("b3:w".into());
        s.record(&receipt("b3:w", Verdict::Correct, 100));
        s.record(&receipt("b3:w", Verdict::Correct, 100));
        s.record(&receipt("b3:w", Verdict::Incorrect, 100));
        let r = s.reputation(&w, 100).unwrap();
        assert!((r - 2.0 / 3.0).abs() < 1e-9, "got {r}");
    }

    #[test]
    fn recency_weights_recent_more() {
        let s = store();
        let w = NodeId("b3:w".into());
        // old incorrect, recent correct; with decay the recent should dominate.
        s.record(&receipt("b3:w", Verdict::Incorrect, 0));
        s.record(&receipt("b3:w", Verdict::Correct, 7 * 24 * 3600));
        // now equals the recent ts; old observation is one half-life back.
        let r = s.reputation(&w, 7 * 24 * 3600).unwrap();
        assert!(r > 0.6, "recent correct should dominate, got {r}");
    }

    #[test]
    fn unknown_worker_has_no_reputation() {
        let s = store();
        assert!(s.reputation(&NodeId("b3:nope".into()), 0).is_none());
    }

    #[test]
    fn observation_history_is_bounded() {
        let trust = TrustConfig::default();
        let limits = LimitsConfig {
            receipt_cache_per_worker: 4,
            ..LimitsConfig::default()
        };
        let s = InMemoryTrustStore::new(&trust, &limits);
        for _ in 0..100 {
            s.record(&receipt("b3:w", Verdict::Correct, 1));
        }
        assert_eq!(s.observation_count(&NodeId("b3:w".into())), 4);
    }

    #[test]
    fn worker_count_is_bounded_with_eviction() {
        let trust = TrustConfig::default();
        let limits = LimitsConfig {
            trust_store_capacity: 3,
            ..LimitsConfig::default()
        };
        let s = InMemoryTrustStore::new(&trust, &limits);
        for i in 0..10 {
            s.record(&receipt(&format!("b3:w{i}"), Verdict::Correct, 1));
        }
        assert_eq!(s.tracked_workers(), 3);
    }

    #[test]
    fn confidence_reputation_penalizes_thin_history() {
        // A "perfect" newcomer (few obs) must score well below a long-correct node.
        let newbie = confidence_reputation(1.0, 3, 1.0, 2.0, 1.96);
        let veteran = confidence_reputation(1.0, 500, 1.0, 2.0, 1.96);
        assert!(
            newbie < veteran,
            "newbie {newbie} should be < veteran {veteran}"
        );
        assert!(newbie < 1.0, "a thinly-observed node is not fully trusted");
        assert!(veteran > 0.9, "a long correct history approaches 1.0");
    }

    #[test]
    fn confidence_reputation_is_monotonic_in_observations() {
        let priors = (1.0, 2.0, 1.96);
        let a = confidence_reputation(0.9, 5, priors.0, priors.1, priors.2);
        let b = confidence_reputation(0.9, 50, priors.0, priors.1, priors.2);
        let c = confidence_reputation(0.9, 5000, priors.0, priors.1, priors.2);
        assert!(
            a < b && b < c,
            "more observations at the same ratio ⇒ higher confidence"
        );
        // Converges to the raw ratio with overwhelming evidence.
        assert!((c - 0.9).abs() < 0.02);
    }

    #[test]
    fn confidence_reputation_z_zero_is_beta_posterior_mean() {
        // With z = 0 the estimate is exactly the prior-shrunk posterior mean.
        // 3 successes + 1 pseudo-success / (3 + 1 + 2) = 4/6.
        let p = confidence_reputation(1.0, 3, 1.0, 2.0, 0.0);
        assert!((p - 4.0 / 6.0).abs() < 1e-9, "got {p}");
    }

    #[test]
    fn exploration_bonus_decays_with_observations() {
        // Off by default (rate = 0).
        assert_eq!(exploration_bonus(0, 0.0, 20), 0.0);
        // A brand-new node gets the full bonus; it decays to 0 at saturation.
        let fresh = exploration_bonus(0, 0.2, 20);
        let mid = exploration_bonus(10, 0.2, 20);
        let saturated = exploration_bonus(20, 0.2, 20);
        let beyond = exploration_bonus(100, 0.2, 20);
        assert!((fresh - 0.2).abs() < 1e-9);
        assert!(mid < fresh && mid > saturated);
        assert_eq!(saturated, 0.0);
        assert_eq!(beyond, 0.0);
    }

    #[test]
    fn confident_reputation_trait_method_uses_priors() {
        let s = store();
        s.record(&receipt("b3:w", Verdict::Correct, 100));
        s.record(&receipt("b3:w", Verdict::Correct, 100));
        s.record(&receipt("b3:w", Verdict::Correct, 100));
        let w = NodeId("b3:w".into());
        // Raw getter is unchanged (still the plain ratio).
        assert_eq!(s.reputation(&w, 100), Some(1.0));
        // Confidence-aware view shrinks the 3-for-3 newcomer below 1.0.
        let c = s.confident_reputation(&w, 100, 1.0, 2.0, 1.96).unwrap();
        assert!(c < 1.0 && c > 0.0, "got {c}");
        assert!(s
            .confident_reputation(&NodeId("b3:none".into()), 100, 1.0, 2.0, 1.96)
            .is_none());
    }

    #[test]
    fn observe_capability_ratchets_maxima_dedups_and_ignores_failures() {
        let s = store();
        let w = NodeId("b3:w".into());
        assert!(s.proven_capability(&w).is_none(), "no history => None");

        let mut r = receipt("b3:w", Verdict::Correct, 100);
        r.observed_input_bytes = 1_000;
        r.observed_result_rows = 50;
        r.observed_result_bytes = 2_000;
        s.observe_capability(&r);
        s.observe_capability(&r); // replay of the SAME receipt is deduped
        let cap = s.proven_capability(&w).unwrap();
        assert_eq!(cap.successes, 1, "replay must not inflate the success count");
        assert_eq!(cap.max_input_bytes, 1_000);
        assert_eq!(cap.max_result_rows, 50);
        assert_eq!(cap.max_result_bytes, 2_000);

        // A distinct, smaller success bumps the count but never lowers the maxima.
        let mut r2 = receipt("b3:w", Verdict::Correct, 200);
        r2.observed_result_rows = 10;
        s.observe_capability(&r2);
        let cap = s.proven_capability(&w).unwrap();
        assert_eq!(cap.successes, 2);
        assert_eq!(cap.max_result_rows, 50, "maxima ratchet up only");

        // A non-Correct verdict never contributes to proven capability.
        s.observe_capability(&receipt("b3:w", Verdict::Incorrect, 300));
        assert_eq!(s.proven_capability(&w).unwrap().successes, 2);

        // Confidence is positive once proven, zero for an unknown peer, and grows
        // with more measured successes.
        let c2 = s.capability_confidence(&w, 1.0, 2.0, 1.96);
        assert!(c2 > 0.0 && c2 < 1.0);
        assert_eq!(
            s.capability_confidence(&NodeId("b3:none".into()), 1.0, 2.0, 1.96),
            0.0
        );
    }

    #[test]
    fn observe_perf_builds_ewma_dedups_and_ignores_failures() {
        let s = store();
        let w = NodeId("b3:w".into());
        assert!(s.perf_aggregate(&w).is_none(), "no history => None");

        // First verified success seeds the EWMA exactly.
        let mut r = receipt("b3:w", Verdict::Correct, 1000);
        r.latency_ms = 100;
        r.observed_input_bytes = 1_000_000; // 1 MB in 100 ms => 10 MB/s
        s.observe_perf(&r);
        s.observe_perf(&r); // replay of the SAME receipt is deduped
        let p = s.perf_aggregate(&w).unwrap();
        assert_eq!(p.obs_count, 1, "replay must not inflate the sample count");
        assert!((p.ewma_latency_ms - 100.0).abs() < 1e-9);
        assert!((p.ewma_bytes - 1_000_000.0).abs() < 1e-9);
        assert!((p.ewma_throughput_bps - 10_000_000.0).abs() < 1e-6);

        // A distinct, slower sample moves the EWMA toward the new value but the
        // count increments by exactly one.
        let mut r2 = receipt("b3:w", Verdict::Correct, 1000 + 7 * 24 * 3600);
        r2.latency_ms = 300;
        r2.observed_input_bytes = 1_000_000;
        s.observe_perf(&r2);
        let p2 = s.perf_aggregate(&w).unwrap();
        assert_eq!(p2.obs_count, 2);
        assert!(
            p2.ewma_latency_ms > 100.0 && p2.ewma_latency_ms <= 300.0,
            "EWMA latency should move toward the new sample, got {}",
            p2.ewma_latency_ms
        );

        // A non-Correct verdict never contributes a perf sample.
        s.observe_perf(&receipt("b3:w", Verdict::Incorrect, 2000));
        assert_eq!(s.perf_aggregate(&w).unwrap().obs_count, 2);
    }

    #[test]
    fn attestation_gate_enforces_minimum() {
        assert!(attestation_gate(AttestationLevel::L2, AttestationLevel::L1));
        assert!(attestation_gate(AttestationLevel::L1, AttestationLevel::L1));
        assert!(!attestation_gate(
            AttestationLevel::L0,
            AttestationLevel::L1
        ));
    }

    #[test]
    fn soft_score_clamps_and_penalizes() {
        let w = ReputationWeights::default();
        let high = soft_trust_score(
            &w,
            &TrustInputs {
                reputation: 1.0,
                age_factor: 1.0,
                voucher_trust: 1.0,
                stake_factor: 1.0,
                penalties: 0.0,
            },
        );
        assert!((high - 1.0).abs() < 1e-9);
        let penalized = soft_trust_score(
            &w,
            &TrustInputs {
                reputation: 1.0,
                age_factor: 1.0,
                voucher_trust: 1.0,
                stake_factor: 1.0,
                penalties: 5.0,
            },
        );
        assert_eq!(penalized, 0.0);
    }
}
