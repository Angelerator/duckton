//! Per-call SQL override structs — the highest-precedence config layer.
//!
//! These map directly to the named arguments of the SQL surface
//! (`p2p_query` / `p2p_share` / `p2p_join`, architecture §12) and are applied
//! on top of a fully-loaded [`crate::GridConfig`] for the duration of one call.
//! Every field is optional; `None` means "inherit the resolved config value".

use serde::{Deserialize, Serialize};

use crate::{
    CompressionAlgo, ConfigError, DataClassCfg, GridConfig, PaymentPref, PreferMode, RegionTrust,
    VerifyModeCfg,
};

/// Overrides for a single `p2p_query(...)` call.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QueryOverrides {
    pub replicas: Option<usize>,
    pub quorum: Option<usize>,
    pub min_trust: Option<f64>,
    pub min_attestation: Option<String>,
    pub verify: Option<VerifyModeCfg>,
    pub dispatch_timeout_ms: Option<u64>,
    /// Per-call requester per-attempt deadline (resilience layer).
    pub attempt_deadline_ms: Option<u64>,
    /// Per-call max (re)dispatch attempts (`0` = unlimited).
    pub max_retries: Option<u32>,
    /// Per-call wall-clock cap (ms) on the whole resilient re-dispatch loop
    /// (`0` = no cap).
    pub max_total_duration_ms: Option<u64>,
    /// Number of concurrent result-transfer streams for this call.
    pub result_parallelism: Option<usize>,
    /// Wire compression algorithm for this call's result.
    pub compression: Option<CompressionAlgo>,
    /// Per-call routing preference (`prefer => local|remote|auto`). Highest
    /// precedence — overrides `planner.prefer` for this query only.
    pub prefer: Option<PreferMode>,
    /// Per-call payment mode (`payment => free|paid|auto`). Highest precedence —
    /// folds into `economics.default_payment` for this query only. `free` ⇒ NO
    /// chain interaction (no escrow/stake/anchor/fees); the job is still scored.
    pub payment: Option<PaymentPref>,
    /// Per-call data class (`data_class => public|internal|sensitive`). Drives the
    /// worker admission gate and the attestation/min-trust selection floors.
    /// `None`/`public` ⇒ unchanged public-tier behavior (back-compatible).
    pub data_class: Option<DataClassCfg>,
    /// Per-call security gate (`require_staked_hosts => true`): restrict selection
    /// to bonded/staked hosts. `None` ⇒ inherit `scheduler.require_staked_hosts`.
    pub require_staked_hosts: Option<bool>,
    /// Per-call **logical grid partition** to target (`network => '...'`). `None`
    /// ⇒ no network constraint (matches any host). NOT the TON chain selector.
    pub network: Option<String>,
    /// Per-call **group** claims (`groups => ['...']`): a grouped host serves the
    /// query only if it shares ≥1 group; an ungrouped host ignores it. Empty ⇒
    /// inherit the node's `[membership].groups`.
    pub groups: Vec<String>,
    /// Per-call **region** constraint (`regions => ['...']`): only hosts in one of
    /// these regions are eligible (fail-closed on a host with no region). A
    /// non-empty list raises the attestation floor under `region_trust = attested`
    /// (Phase 4). Empty ⇒ no region constraint.
    pub regions: Vec<String>,
    /// Per-call **target node(s)** (`nodes => ['b3:...']`): restrict the job to
    /// these EXACT node ids — the coordinator only offers to and dispatches to
    /// candidates whose known id is in this set (fail-closed: a candidate with no
    /// known id can never be a target). Combine with `replicas => 1, quorum => 1`
    /// to send the whole job to one specific node, or `prefer => 'local'` to run
    /// it on the requester itself. Empty ⇒ no node constraint (normal routing).
    pub nodes: Vec<String>,
}

/// Rank of an attestation tier string for floor comparison (`L0 < L1 < L2`).
fn attestation_rank(level: &str) -> u8 {
    match level.trim().to_ascii_uppercase().as_str() {
        "L2" => 2,
        "L1" => 1,
        _ => 0,
    }
}

impl QueryOverrides {
    /// Apply these overrides onto a config, returning the effective config.
    /// Re-validates so per-call params can't produce an illegal config.
    pub fn apply(&self, base: &GridConfig) -> Result<GridConfig, ConfigError> {
        let mut cfg = base.clone();
        if let Some(r) = self.replicas {
            cfg.scheduler.replicas = r;
        }
        if let Some(q) = self.quorum {
            cfg.scheduler.quorum = q;
        }
        if let Some(t) = self.min_trust {
            cfg.trust.min_trust = t;
        }
        if let Some(a) = &self.min_attestation {
            cfg.trust.min_attestation = a.clone();
        }
        if let Some(v) = self.verify {
            cfg.scheduler.verify_mode = v;
        }
        if let Some(d) = self.dispatch_timeout_ms {
            cfg.scheduler.dispatch_timeout_ms = d;
        }
        if let Some(d) = self.attempt_deadline_ms {
            cfg.scheduler.attempt_deadline_ms = d;
        }
        if let Some(r) = self.max_retries {
            cfg.scheduler.max_retries = r;
        }
        if let Some(d) = self.max_total_duration_ms {
            cfg.scheduler.max_total_duration_ms = d;
        }
        if let Some(p) = self.result_parallelism {
            cfg.transport.result.parallelism = p;
            // The uni-stream cap must accommodate the requested fan-out.
            if cfg.transport.quic.max_concurrent_uni_streams < p as u32 {
                cfg.transport.quic.max_concurrent_uni_streams = p as u32;
            }
        }
        if let Some(c) = self.compression {
            cfg.transport.compression.algorithm = c;
        }
        if let Some(p) = self.prefer {
            cfg.planner.prefer = p;
        }
        if let Some(pm) = self.payment {
            // Fold the per-call payment choice into the resolved config; the
            // coordinator then resolves the final free/paid mode per data class.
            cfg.economics.default_payment = pm;
        }
        if let Some(b) = self.require_staked_hosts {
            cfg.scheduler.require_staked_hosts = b;
        }
        if let Some(dc) = self.data_class {
            // Apply the class selection floors (raise-only): a higher class needs
            // a stronger attestation tier + min trust; an explicit per-call
            // min_attestation/min_trust above the floor still wins.
            if let Some((floor_att, floor_trust)) = dc.selection_floor() {
                if attestation_rank(floor_att) > attestation_rank(&cfg.trust.min_attestation) {
                    cfg.trust.min_attestation = floor_att.to_string();
                }
                if floor_trust > cfg.trust.min_trust {
                    cfg.trust.min_trust = floor_trust;
                }
            }
        }
        // A region constraint raises the attestation floor "like data_class" — but
        // ONLY under the attested region-trust tier (Phase 4). With the default
        // `declared` tier the host's region is trusted as declared, so the floor is
        // left unchanged (keeps region routing usable on today's L0 hosts). Empty
        // regions ⇒ no constraint, unchanged behavior.
        if !self.regions.is_empty() && cfg.membership.region_trust == RegionTrust::Attested {
            // Mirror the data_class Internal floor (L1): region residency under the
            // attested tier needs at least measured-boot evidence.
            if attestation_rank("L1") > attestation_rank(&cfg.trust.min_attestation) {
                cfg.trust.min_attestation = "L1".to_string();
            }
        }
        // candidate_sample_size must stay >= replicas; widen if needed.
        if cfg.discovery.candidate_sample_size < cfg.scheduler.replicas {
            cfg.discovery.candidate_sample_size = cfg.scheduler.replicas;
        }
        cfg.validate()?;
        Ok(cfg)
    }
}

/// Overrides for a `p2p_share(...)` call (becoming a host).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ShareOverrides {
    pub memory_bytes: Option<u64>,
    pub threads: Option<u32>,
    pub max_jobs: Option<u32>,
    pub data_classes: Option<Vec<DataClassCfg>>,
    /// Host serving state (`enabled => false` ⇒ graceful standby/drain).
    pub enabled: Option<bool>,
    /// Logical grid partitions this host serves (`networks => ['...']`).
    pub networks: Option<Vec<String>>,
    /// Group memberships this host serves (`groups => ['...']`).
    pub groups: Option<Vec<String>>,
    /// Declared region of this host (`region => '...'`; empty string ⇒ clear).
    pub region: Option<String>,
}

impl ShareOverrides {
    pub fn apply(&self, base: &GridConfig) -> Result<GridConfig, ConfigError> {
        let mut cfg = base.clone();
        if let Some(m) = self.memory_bytes {
            cfg.budget.memory_bytes = m;
        }
        if let Some(t) = self.threads {
            cfg.budget.threads = t;
        }
        if let Some(j) = self.max_jobs {
            cfg.budget.max_jobs = j;
        }
        if let Some(dc) = &self.data_classes {
            cfg.budget.data_classes = dc.clone();
        }
        if let Some(e) = self.enabled {
            cfg.worker.enabled = e;
        }
        if let Some(n) = &self.networks {
            cfg.membership.networks = n.clone();
        }
        if let Some(g) = &self.groups {
            cfg.membership.groups = g.clone();
        }
        if let Some(r) = &self.region {
            cfg.membership.region = if r.trim().is_empty() {
                None
            } else {
                Some(r.clone())
            };
        }
        cfg.validate()?;
        Ok(cfg)
    }
}

/// Overrides for a `p2p_join(...)` call (entering the swarm).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JoinOverrides {
    pub bootstrap: Option<Vec<String>>,
}

impl JoinOverrides {
    pub fn apply(&self, base: &GridConfig) -> Result<GridConfig, ConfigError> {
        let mut cfg = base.clone();
        if let Some(b) = &self.bootstrap {
            cfg.discovery.bootstrap = b.clone();
        }
        cfg.validate()?;
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_override_is_highest_precedence() {
        let base = GridConfig::default(); // replicas=3, quorum=2
        let ov = QueryOverrides {
            replicas: Some(5),
            quorum: Some(4),
            min_trust: Some(0.9),
            ..Default::default()
        };
        let eff = ov.apply(&base).unwrap();
        assert_eq!(eff.scheduler.replicas, 5);
        assert_eq!(eff.scheduler.quorum, 4);
        assert_eq!(eff.trust.min_trust, 0.9);
        // sample size auto-widened to satisfy invariant
        assert!(eff.discovery.candidate_sample_size >= 5);
    }

    #[test]
    fn query_override_revalidates() {
        let base = GridConfig::default();
        let ov = QueryOverrides {
            replicas: Some(2),
            quorum: Some(3), // illegal: quorum > replicas
            ..Default::default()
        };
        assert!(ov.apply(&base).is_err());
    }

    #[test]
    fn query_override_prefer_is_highest_precedence() {
        // Base config (from defaults) has planner.prefer = auto.
        let base = GridConfig::default();
        assert_eq!(base.planner.prefer, PreferMode::Auto);
        let eff = QueryOverrides {
            prefer: Some(PreferMode::Local),
            ..Default::default()
        }
        .apply(&base)
        .unwrap();
        assert_eq!(eff.planner.prefer, PreferMode::Local);
        // Absent override inherits the resolved config value.
        let eff2 = QueryOverrides::default().apply(&base).unwrap();
        assert_eq!(eff2.planner.prefer, PreferMode::Auto);
    }

    #[test]
    fn per_call_prefer_overrides_sticky_config_default_both_ways() {
        // Sticky remote-grid default in config; a per-call `prefer => local`
        // overrides it (and vice-versa), proving per-call wins over the runtime
        // layer regardless of direction.
        let mut remote_default = GridConfig::default();
        remote_default.planner.prefer = PreferMode::Remote;
        let eff = QueryOverrides {
            prefer: Some(PreferMode::Local),
            ..Default::default()
        }
        .apply(&remote_default)
        .unwrap();
        assert_eq!(eff.planner.prefer, PreferMode::Local);

        let mut local_default = GridConfig::default();
        local_default.planner.prefer = PreferMode::Local;
        let eff = QueryOverrides {
            prefer: Some(PreferMode::Remote),
            ..Default::default()
        }
        .apply(&local_default)
        .unwrap();
        assert_eq!(eff.planner.prefer, PreferMode::Remote);
    }

    #[test]
    fn data_class_applies_selection_floors_raise_only() {
        let base = GridConfig::default();
        // Public (or unset) leaves the trust gates unchanged.
        let pub_eff = QueryOverrides {
            data_class: Some(DataClassCfg::Public),
            ..Default::default()
        }
        .apply(&base)
        .unwrap();
        assert_eq!(pub_eff.trust.min_attestation, base.trust.min_attestation);
        assert_eq!(pub_eff.trust.min_trust, base.trust.min_trust);

        // Sensitive raises the attestation floor to L2 and min_trust to the floor.
        let sens = QueryOverrides {
            data_class: Some(DataClassCfg::Sensitive),
            ..Default::default()
        }
        .apply(&base)
        .unwrap();
        assert_eq!(sens.trust.min_attestation, "L2");
        assert!(sens.trust.min_trust >= 0.80);

        // An explicit higher min_trust still wins (floor only raises).
        let higher = QueryOverrides {
            data_class: Some(DataClassCfg::Internal),
            min_trust: Some(0.95),
            ..Default::default()
        }
        .apply(&base)
        .unwrap();
        assert_eq!(higher.trust.min_attestation, "L1");
        assert_eq!(higher.trust.min_trust, 0.95);
    }

    #[test]
    fn share_override_sets_budget() {
        let eff = ShareOverrides {
            memory_bytes: Some(8 << 30),
            threads: Some(4),
            max_jobs: Some(5),
            data_classes: Some(vec![DataClassCfg::Public, DataClassCfg::Internal]),
            ..Default::default()
        }
        .apply(&GridConfig::default())
        .unwrap();
        assert_eq!(eff.budget.threads, 4);
        assert_eq!(eff.budget.data_classes.len(), 2);
    }

    #[test]
    fn query_override_region_raises_floor_only_under_attested_tier() {
        let ov = QueryOverrides {
            network: Some("eu".into()),
            groups: vec!["finance".into()],
            regions: vec!["eu".into()],
            ..Default::default()
        };
        // Default region_trust = declared ⇒ a region constraint does NOT raise the
        // attestation floor (keeps region routing usable on today's L0 hosts).
        let base = GridConfig::default();
        let eff = ov.apply(&base).unwrap();
        assert_eq!(eff.trust.min_attestation, base.trust.min_attestation);

        // Attested tier (Phase 4) ⇒ a region constraint raises the floor to L1.
        let mut attested = GridConfig::default();
        attested.membership.region_trust = RegionTrust::Attested;
        let eff = ov.apply(&attested).unwrap();
        assert_eq!(eff.trust.min_attestation, "L1");
        // No region constraint ⇒ floor unchanged even under the attested tier.
        let eff2 = QueryOverrides::default().apply(&attested).unwrap();
        assert_eq!(eff2.trust.min_attestation, attested.trust.min_attestation);
    }

    #[test]
    fn share_override_sets_membership_labels() {
        let eff = ShareOverrides {
            enabled: Some(false),
            networks: Some(vec!["eu".into(), "default".into()]),
            groups: Some(vec!["finance".into()]),
            region: Some("eu".into()),
            ..Default::default()
        }
        .apply(&GridConfig::default())
        .unwrap();
        assert!(!eff.worker.enabled);
        assert_eq!(
            eff.membership.networks,
            vec!["eu".to_string(), "default".to_string()]
        );
        assert_eq!(eff.membership.groups, vec!["finance".to_string()]);
        assert_eq!(eff.membership.region.as_deref(), Some("eu"));

        // An empty region string clears the declared region back to None.
        let cleared = ShareOverrides {
            region: Some(String::new()),
            ..Default::default()
        }
        .apply(&GridConfig::default())
        .unwrap();
        assert_eq!(cleared.membership.region, None);
    }
}
