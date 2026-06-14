//! Per-call SQL override structs — the highest-precedence config layer.
//!
//! These map directly to the named arguments of the SQL surface
//! (`p2p_query` / `p2p_share` / `p2p_join`, architecture §12) and are applied
//! on top of a fully-loaded [`crate::GridConfig`] for the duration of one call.
//! Every field is optional; `None` means "inherit the resolved config value".

use serde::{Deserialize, Serialize};

use crate::{
    CompressionAlgo, ConfigError, DataClassCfg, GridConfig, PaymentPref, PreferMode, VerifyModeCfg,
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
    fn share_override_sets_budget() {
        let eff = ShareOverrides {
            memory_bytes: Some(8 << 30),
            threads: Some(4),
            max_jobs: Some(5),
            data_classes: Some(vec![DataClassCfg::Public, DataClassCfg::Internal]),
        }
        .apply(&GridConfig::default())
        .unwrap();
        assert_eq!(eff.budget.threads, 4);
        assert_eq!(eff.budget.data_classes.len(), 2);
    }
}
