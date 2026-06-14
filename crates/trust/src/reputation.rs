//! Reputation & trust scoring (architecture §7.3 / §7.5).
//!
//! Reputation is a **recency-weighted correctness rate** built from verified
//! receipts:  `R = Σ wᵢ·correctᵢ / Σ wᵢ`,  `wᵢ = decay^age · job_weight`.
//!
//! The store is **pluggable** behind [`TrustStore`] so tests use the in-memory
//! implementation while production can use a persistent embedded store. All
//! caches are **bounded** (per [`p2p_config::LimitsConfig`]) — no unbounded maps.

use std::collections::{HashMap, VecDeque};
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

/// One recorded outcome for a worker.
#[derive(Debug, Clone, Copy)]
struct Observation {
    ts: u64,
    correct: bool,
    weight: f64,
}

/// Pluggable reputation/receipt store.
///
/// Implementations must be cheap to share (`Arc<dyn TrustStore>`); methods take
/// `&self` and handle their own interior synchronization.
pub trait TrustStore: Send + Sync {
    /// Ingest a verified receipt (caller must have checked the signature).
    fn record(&self, receipt: &Receipt);
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
        Some(confidence_reputation(ratio, obs, prior_alpha, prior_beta, z))
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
    voucher_trust: f64,
    penalty: f64,
}

impl WorkerState {
    fn new() -> Self {
        Self {
            observations: VecDeque::new(),
            voucher_trust: 0.0,
            penalty: 0.0,
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
        self.with_worker(&receipt.worker_id, |state| {
            state.observations.push_back(Observation {
                ts: receipt.ts,
                correct,
                weight: 1.0,
            });
            while state.observations.len() > cap {
                state.observations.pop_front();
            }
        });
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
        hist.push_back(Observation { ts, correct, weight: 1.0 });
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
    let margin = z * ((p * (1.0 - p) / trials) + z2 / (4.0 * trials * trials)).max(0.0).sqrt();
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
            requester_pubkey: String::new(),
            sig: String::new(),
        }
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
        assert!(newbie < veteran, "newbie {newbie} should be < veteran {veteran}");
        assert!(newbie < 1.0, "a thinly-observed node is not fully trusted");
        assert!(veteran > 0.9, "a long correct history approaches 1.0");
    }

    #[test]
    fn confidence_reputation_is_monotonic_in_observations() {
        let priors = (1.0, 2.0, 1.96);
        let a = confidence_reputation(0.9, 5, priors.0, priors.1, priors.2);
        let b = confidence_reputation(0.9, 50, priors.0, priors.1, priors.2);
        let c = confidence_reputation(0.9, 5000, priors.0, priors.1, priors.2);
        assert!(a < b && b < c, "more observations at the same ratio ⇒ higher confidence");
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
        assert!(s.confident_reputation(&NodeId("b3:none".into()), 100, 1.0, 2.0, 1.96).is_none());
    }

    #[test]
    fn attestation_gate_enforces_minimum() {
        assert!(attestation_gate(AttestationLevel::L2, AttestationLevel::L1));
        assert!(attestation_gate(AttestationLevel::L1, AttestationLevel::L1));
        assert!(!attestation_gate(AttestationLevel::L0, AttestationLevel::L1));
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
