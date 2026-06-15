//! `[antiabuse]` — the anti-abuse / robustness layer configuration (defends the
//! grid against reputation-griefing and other abuse, ARCHITECTURE "Abuse
//! resistance").
//!
//! Every value has a documented default. The defaults are chosen to **preserve
//! today's behavior** where a change would be observable: the scoring-altering
//! mechanisms (requester-trust weighting, pre-flight cost gating, free-mode rate
//! limiting, auto-blocking, gossip peer scoring) default **off**; only the
//! always-safe pieces (provable-fault attribution and non-determinism detection,
//! which never *add* a penalty) default on. Operators opt into the rest.
//!
//! Layers like everything else: defaults → TOML (`[antiabuse]`) →
//! `P2P_ANTIABUSE_*` env → per-call. Nothing is hard-coded.

use serde::{Deserialize, Serialize};

use crate::ConfigError;

/// Top-level `[antiabuse]` section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AntiAbuseConfig {
    /// Master switch. `false` disables every sub-mechanism (exactly today's
    /// behavior). `true` (default) lets each sub-mechanism's own flag decide.
    pub enabled: bool,
    pub fault_attribution: FaultAttributionConfig,
    pub requester_trust: RequesterTrustConfig,
    pub cost_gate: CostGateConfig,
    pub nondeterminism: NondeterminismConfig,
    pub free_rate_limit: FreeRateLimitConfig,
    pub blocklist: BlocklistPolicyConfig,
    pub gossip: GossipHardeningConfig,
}

impl Default for AntiAbuseConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            fault_attribution: FaultAttributionConfig::default(),
            requester_trust: RequesterTrustConfig::default(),
            cost_gate: CostGateConfig::default(),
            nondeterminism: NondeterminismConfig::default(),
            free_rate_limit: FreeRateLimitConfig::default(),
            blocklist: BlocklistPolicyConfig::default(),
            gossip: GossipHardeningConfig::default(),
        }
    }
}

/// `[antiabuse.fault_attribution]` — penalize a provider ONLY for provable
/// provider fault (result disagrees with a verified quorum, downtime,
/// equivocation). Requester/job-caused failures (infeasible / too expensive /
/// resource-exceeded / malformed / missing data) apply **zero** provider
/// penalty. A consensus signal (most/all selected providers fail the SAME way)
/// attributes the failure to the JOB, not the providers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FaultAttributionConfig {
    /// Apply fault attribution. Default on (it only ever *removes* an unfair
    /// penalty; a genuine quorum-minority cheater is still penalized).
    pub enabled: bool,
    /// If at least this fraction of the selected providers fail the **same** way
    /// (e.g. all time out / all OOM) and no quorum forms, attribute the failure
    /// to the job rather than penalizing the providers.
    pub job_consensus_fraction: f64,
}

impl Default for FaultAttributionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            job_consensus_fraction: 0.67,
        }
    }
}

/// `[antiabuse.requester_trust]` — trust-weighted score impact ("newer sender →
/// less effect"). A job's effect on a provider's reputation/penalty is
/// multiplied by `w(requester) ∈ (0,1]` derived from the requester's own
/// reputation + age. A new/unproven requester ⇒ `w ≈ 0` (especially for negative
/// outcomes); an established requester ⇒ `w → 1`. This is the primary defense
/// against the heavy-query reputation-griefing attack.
///
/// **Default off** because turning it on changes how penalties are scaled. When
/// off, every requester has weight `1.0` (today's behavior).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RequesterTrustConfig {
    pub enabled: bool,
    /// Floor weight applied to a brand-new requester's **negative** outcome
    /// (penalties). `0.0` ⇒ a totally unproven requester cannot move a provider's
    /// score downward at all until it builds its own standing.
    pub negative_floor_weight: f64,
    /// Floor weight applied to a brand-new requester's **positive** outcome
    /// (reputation credit). Higher than the negative floor (asymmetric): we are
    /// far more cautious about letting strangers *hurt* scores than help them.
    pub positive_floor_weight: f64,
    /// Number of the requester's own verified observations at which its weight
    /// saturates to `1.0`.
    pub age_saturation: usize,
}

impl Default for RequesterTrustConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            negative_floor_weight: 0.0,
            positive_floor_weight: 0.5,
            age_saturation: 50,
        }
    }
}

/// `[antiabuse.cost_gate]` — pre-flight cost gating at admission. The worker uses
/// the metadata estimator / the offer's cost hint to **reject** an over-budget
/// query up front. A rejection is not an execution failure and never affects a
/// provider's score (a rejected bid produces no receipt). Default off.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CostGateConfig {
    pub enabled: bool,
    /// Reject an offer whose `cost_hint_rows` exceeds this. `0` = no row cap.
    pub max_cost_hint_rows: u64,
    /// Reject when the estimated peak working set exceeds the per-job memory
    /// budget times this factor. `0` = derive from the budget (factor `1.0`).
    pub max_working_set_factor: f64,
}

impl Default for CostGateConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_cost_hint_rows: 0,
            max_working_set_factor: 1.0,
        }
    }
}

/// `[antiabuse.nondeterminism]` — detect non-deterministic queries (`random()`,
/// `now()`/`current_*`, unordered `LIMIT`, …) that cannot reach a stable quorum
/// hash, mark the job **non-verifiable**, and apply no provider penalty (don't
/// treat a hash mismatch as provider failure). Default on (safe: deterministic
/// queries are unaffected).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NondeterminismConfig {
    pub enabled: bool,
}

impl Default for NondeterminismConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// `[antiabuse.free_rate_limit]` — per-requester-identity rate limiting for FREE
/// jobs (anti-spam). Paid jobs are prioritized and never rate-limited here.
/// Optionally require a small PoW from free requesters. Default off.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FreeRateLimitConfig {
    pub enabled: bool,
    /// Max free offers admitted from one requester identity per `window_secs`.
    pub max_free_per_window: u32,
    /// Sliding window length (secs).
    pub window_secs: u64,
    /// Bounded number of distinct requester identities tracked (LRU-evicted).
    pub max_tracked_requesters: usize,
    /// Optional PoW difficulty (leading zero bits) a free requester must satisfy.
    /// `0` = no PoW required. (Heuristic; full wire enforcement needs the offer
    /// to carry a PoW stamp — see ARCHITECTURE "Abuse resistance".)
    pub require_pow_bits: u32,
    /// Prioritize paid jobs over free jobs (paid jobs bypass the free limiter).
    pub prioritize_paid: bool,
}

impl Default for FreeRateLimitConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_free_per_window: 30,
            window_secs: 60,
            max_tracked_requesters: 10_000,
            require_pow_bits: 0,
            prioritize_paid: true,
        }
    }
}

/// `[antiabuse.blocklist]` — local deny-list policy. Entries are managed via the
/// SQL admin surface (`p2p_block` / `p2p_unblock` / `p2p_blocklist`) and persist
/// in a dedicated `blocklist.toml`. This section governs the **automatic** block
/// triggers and which external block sources are honored. Each node decides
/// independently — there is no central authority.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BlocklistPolicyConfig {
    /// Auto-block a worker whose effective trust falls strictly below
    /// `auto_block_trust_floor`. Default off.
    pub auto_block_enabled: bool,
    /// Trust floor for auto-blocking. `0.0` disables the floor even when
    /// `auto_block_enabled` (so only explicit/slashing blocks apply).
    pub auto_block_trust_floor: f64,
    /// Honor signed, gossiped abuse signals: a node receiving a verified signal
    /// about an actor may independently refuse it. Default off.
    pub honor_gossip_signals: bool,
    /// Consult the on-chain `GlobalParams` governance blocklist (admin-gated, for
    /// egregious provable cases). Default off (purely local otherwise).
    pub honor_global_params: bool,
}

impl Default for BlocklistPolicyConfig {
    fn default() -> Self {
        Self {
            auto_block_enabled: false,
            auto_block_trust_floor: 0.0,
            honor_gossip_signals: false,
            honor_global_params: false,
        }
    }
}

/// `[antiabuse.gossip]` — eclipse / gossip hardening for the libp2p discovery
/// overlay. Enables gossipsub peer scoring and encourages diverse bootstrap/peer
/// selection so the swarm resists eclipse attacks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GossipHardeningConfig {
    /// Enable gossipsub peer scoring (penalize misbehaving mesh peers). Default
    /// off so small loopback meshes are never pruned unexpectedly; enable it on
    /// real swarms.
    pub peer_scoring: bool,
    /// Prefer a diverse set of bootstrap/relay peers (avoid relying on a single
    /// provider that could eclipse the node). Default on (advisory selection).
    pub diverse_bootstrap: bool,
}

impl Default for GossipHardeningConfig {
    fn default() -> Self {
        Self {
            peer_scoring: false,
            diverse_bootstrap: true,
        }
    }
}

impl AntiAbuseConfig {
    /// Validate ranges / cross-field invariants.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let pct = |name: &str, x: f64| -> Result<(), ConfigError> {
            if !(0.0..=1.0).contains(&x) {
                return Err(ConfigError::Invalid(format!(
                    "antiabuse.{name} must be in [0,1], got {x}"
                )));
            }
            Ok(())
        };
        pct(
            "fault_attribution.job_consensus_fraction",
            self.fault_attribution.job_consensus_fraction,
        )?;
        pct(
            "requester_trust.negative_floor_weight",
            self.requester_trust.negative_floor_weight,
        )?;
        pct(
            "requester_trust.positive_floor_weight",
            self.requester_trust.positive_floor_weight,
        )?;
        if self.requester_trust.positive_floor_weight < self.requester_trust.negative_floor_weight {
            return Err(ConfigError::Invalid(
                "antiabuse.requester_trust.positive_floor_weight must be >= negative_floor_weight \
                 (gate negatives at least as hard as positives)"
                    .into(),
            ));
        }
        pct(
            "cost_gate.max_working_set_factor",
            self.cost_gate.max_working_set_factor.min(1.0),
        )
        .ok(); // factor may exceed 1.0; only guard against negatives below
        if self.cost_gate.max_working_set_factor < 0.0 {
            return Err(ConfigError::Invalid(
                "antiabuse.cost_gate.max_working_set_factor must be >= 0".into(),
            ));
        }
        pct(
            "blocklist.auto_block_trust_floor",
            self.blocklist.auto_block_trust_floor,
        )?;
        if self.free_rate_limit.enabled {
            if self.free_rate_limit.window_secs == 0 {
                return Err(ConfigError::Invalid(
                    "antiabuse.free_rate_limit.window_secs must be >= 1 when enabled".into(),
                ));
            }
            if self.free_rate_limit.max_free_per_window == 0 {
                return Err(ConfigError::Invalid(
                    "antiabuse.free_rate_limit.max_free_per_window must be >= 1 when enabled"
                        .into(),
                ));
            }
            if self.free_rate_limit.max_tracked_requesters == 0 {
                return Err(ConfigError::Invalid(
                    "antiabuse.free_rate_limit.max_tracked_requesters must be >= 1 when enabled"
                        .into(),
                ));
            }
        }
        Ok(())
    }

    /// Whether the requester-trust weighting mechanism is active.
    pub fn requester_trust_active(&self) -> bool {
        self.enabled && self.requester_trust.enabled
    }

    /// Whether fault attribution is active.
    pub fn fault_attribution_active(&self) -> bool {
        self.enabled && self.fault_attribution.enabled
    }

    /// Whether non-determinism detection is active.
    pub fn nondeterminism_active(&self) -> bool {
        self.enabled && self.nondeterminism.enabled
    }

    /// Whether pre-flight cost gating is active.
    pub fn cost_gate_active(&self) -> bool {
        self.enabled && self.cost_gate.enabled
    }

    /// Whether free-mode rate limiting is active.
    pub fn free_rate_limit_active(&self) -> bool {
        self.enabled && self.free_rate_limit.enabled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_behavior_preserving() {
        let a = AntiAbuseConfig::default();
        a.validate().unwrap();
        // Master on, but scoring-altering pieces default off.
        assert!(a.enabled);
        assert!(a.fault_attribution.enabled);
        assert!(a.nondeterminism.enabled);
        assert!(!a.requester_trust.enabled);
        assert!(!a.cost_gate.enabled);
        assert!(!a.free_rate_limit.enabled);
        assert!(!a.blocklist.auto_block_enabled);
        assert!(!a.gossip.peer_scoring);
    }

    #[test]
    fn rejects_positive_floor_below_negative_floor() {
        let mut a = AntiAbuseConfig::default();
        a.requester_trust.negative_floor_weight = 0.5;
        a.requester_trust.positive_floor_weight = 0.1;
        assert!(a.validate().is_err());
    }

    #[test]
    fn rejects_zero_window_when_rate_limit_enabled() {
        let mut a = AntiAbuseConfig::default();
        a.free_rate_limit.enabled = true;
        a.free_rate_limit.window_secs = 0;
        assert!(a.validate().is_err());
    }
}
