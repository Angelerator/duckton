//! Provider **quality score `Q ∈ [0,1]`** (BLOCKCHAIN_ECONOMICS §4.1).
//!
//! `Q` drives ranking *and* settlement. It blends four positive terms — success
//! rate `S`, a **size-normalized latency** score `L`, a **throughput-as-rate**
//! score `T`, and an ETA/completion-honesty score `C` — and then applies any
//! penalties **multiplicatively**, clamping the result to `[0,1]`.
//!
//! Rationality fixes over the naive formula (§4.1/§4.3):
//!   * **Size-normalized latency**: a large job legitimately takes longer, so the
//!     latency allowance scales with the verified data volume. A provider is not
//!     punished for honestly processing a big dataset, and cannot look "fast" by
//!     only ever taking tiny jobs.
//!   * **Throughput as a rate** (bytes/ms), log-scaled with diminishing returns,
//!     instead of raw volume — so a slow provider that merely touches a lot of
//!     bytes does not out-score a genuinely fast one.
//!   * **`Q` is clamped to `[0,1]`** and penalties multiply it down (a 50% canary
//!     penalty halves `Q`) rather than being subtracted (which could push the
//!     raw score negative and break ranking comparisons).

use p2p_config::QualityEconomics;

/// One provider's verified-history sample feeding the quality score.
#[derive(Debug, Clone, PartialEq)]
pub struct QualitySample {
    /// Success rate `S ∈ [0,1]` — the (confidence-aware) correctness rate from
    /// signed receipts (quorum/canary-decided, never self-reported).
    pub success_ratio: f64,
    /// Observed latency (e.g. p95) in milliseconds for the measured work.
    pub latency_ms: u64,
    /// Verified bytes processed (only results that reached the agreed hash count).
    pub bytes_verified: u64,
    /// ETA/completion-honesty score `C ∈ [0,1]` (on-time completions minus an
    /// ETA-deviation penalty). Use `1.0` when not tracked.
    pub completion: f64,
    /// Multiplicative penalty fractions in `[0,1]` (failed canaries, slashes,
    /// downtime). Each multiplies `Q` by `(1 - p)`.
    pub penalties: Vec<f64>,
}

impl QualitySample {
    /// A sample with no completion tracking and no penalties.
    pub fn new(success_ratio: f64, latency_ms: u64, bytes_verified: u64) -> Self {
        Self {
            success_ratio,
            latency_ms,
            bytes_verified,
            completion: 1.0,
            penalties: Vec::new(),
        }
    }
}

/// Effective reference throughput (bytes/ms): the explicit config knob when set,
/// else derived from `bytes_ref / latency_ref_ms`.
fn ref_rate(cfg: &QualityEconomics) -> f64 {
    if cfg.throughput_ref_bytes_per_ms > 0 {
        cfg.throughput_ref_bytes_per_ms as f64
    } else if cfg.latency_ref_ms > 0 {
        (cfg.bytes_ref as f64) / (cfg.latency_ref_ms as f64)
    } else {
        cfg.bytes_ref as f64
    }
}

/// **Size-normalized latency** score `L ∈ [0,1]` (§4.1). The acceptable latency
/// scales up with the verified data volume (floor at the base `latency_ref_ms`
/// for sub-reference jobs), so larger jobs get a proportionally larger allowance.
/// Faster-than-allowance ⇒ closer to 1; at/over the allowance ⇒ 0.
pub fn latency_score(latency_ms: u64, bytes_verified: u64, cfg: &QualityEconomics) -> f64 {
    if cfg.latency_ref_ms == 0 {
        return 1.0;
    }
    let size_factor = if cfg.bytes_ref > 0 {
        (bytes_verified as f64 / cfg.bytes_ref as f64).max(1.0)
    } else {
        1.0
    };
    let effective_ref = cfg.latency_ref_ms as f64 * size_factor;
    (1.0 - latency_ms as f64 / effective_ref).clamp(0.0, 1.0)
}

/// **Throughput-as-rate** score `T ∈ [0,1]` (§4.1): `ln(1+rate)/ln(1+ref_rate)`
/// where `rate = bytes_verified / latency_ms` (bytes per ms). Log-scaled so
/// volume helps with diminishing returns; at the reference rate `T = 1`.
pub fn throughput_score(latency_ms: u64, bytes_verified: u64, cfg: &QualityEconomics) -> f64 {
    let rate = bytes_verified as f64 / (latency_ms.max(1) as f64);
    let refr = ref_rate(cfg);
    if refr <= 0.0 {
        return 0.0;
    }
    let denom = (1.0 + refr).ln();
    if denom <= 0.0 {
        return 0.0;
    }
    ((1.0 + rate).ln() / denom).clamp(0.0, 1.0)
}

/// Compute the provider quality score `Q ∈ [0,1]` (§4.1). Positive terms are a
/// weight-normalized blend; penalties apply multiplicatively; the result is
/// clamped to `[0,1]`.
pub fn quality_score(sample: &QualitySample, cfg: &QualityEconomics) -> f64 {
    let s = sample.success_ratio.clamp(0.0, 1.0);
    let l = latency_score(sample.latency_ms, sample.bytes_verified, cfg);
    let t = throughput_score(sample.latency_ms, sample.bytes_verified, cfg);
    let c = sample.completion.clamp(0.0, 1.0);

    let weight_sum = cfg.w_success + cfg.w_latency + cfg.w_throughput + cfg.w_completion;
    let base = if weight_sum > 0.0 {
        (cfg.w_success * s + cfg.w_latency * l + cfg.w_throughput * t + cfg.w_completion * c)
            / weight_sum
    } else {
        0.0
    };

    // Penalties are multiplicative (a 50% penalty halves Q) — never subtractive,
    // so Q can never go negative and ranking comparisons stay well-defined.
    let mut q = base;
    for p in &sample.penalties {
        q *= (1.0 - p.clamp(0.0, 1.0)).max(0.0);
    }
    q.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> QualityEconomics {
        QualityEconomics::default()
    }

    #[test]
    fn q_is_clamped_to_unit_interval() {
        // Perfect everything, no penalties.
        let perfect = QualitySample {
            success_ratio: 1.0,
            latency_ms: 0,
            bytes_verified: cfg().bytes_ref,
            completion: 1.0,
            penalties: vec![],
        };
        let q = quality_score(&perfect, &cfg());
        assert!((0.0..=1.0).contains(&q));
        assert!(q > 0.8, "near-perfect sample should score high, got {q}");

        // Even absurd inputs stay clamped.
        let weird = QualitySample {
            success_ratio: 5.0,
            latency_ms: 0,
            bytes_verified: u64::MAX,
            completion: 9.0,
            penalties: vec![],
        };
        assert!((0.0..=1.0).contains(&quality_score(&weird, &cfg())));
    }

    #[test]
    fn penalties_apply_multiplicatively() {
        let base = QualitySample::new(1.0, 100, cfg().bytes_ref);
        let q0 = quality_score(&base, &cfg());
        let mut penalized = base.clone();
        penalized.penalties = vec![0.5];
        let q1 = quality_score(&penalized, &cfg());
        assert!((q1 - 0.5 * q0).abs() < 1e-9, "a 50% penalty must halve Q ({q0} -> {q1})");
        // Stacked penalties compound.
        let mut stacked = base.clone();
        stacked.penalties = vec![0.5, 0.5];
        assert!((quality_score(&stacked, &cfg()) - 0.25 * q0).abs() < 1e-9);
    }

    #[test]
    fn latency_is_size_normalized() {
        let c = cfg();
        // A 10x-larger job at 5x the latency still scores BETTER than a tiny job
        // at the same absolute latency, because the allowance scales with size.
        let big = latency_score(5 * c.latency_ref_ms, 10 * c.bytes_ref, &c);
        let small = latency_score(5 * c.latency_ref_ms, c.bytes_ref / 1000, &c);
        assert!(big > small, "big job ({big}) should not be punished vs tiny ({small})");
        // Sub-reference jobs use the base latency_ref (floor at size_factor 1).
        assert_eq!(
            latency_score(c.latency_ref_ms, 1, &c),
            latency_score(c.latency_ref_ms, c.bytes_ref / 2, &c)
        );
    }

    #[test]
    fn throughput_is_a_rate_not_raw_volume() {
        let c = cfg();
        // Same bytes, faster (lower latency) ⇒ higher throughput score.
        let fast = throughput_score(100, c.bytes_ref, &c);
        let slow = throughput_score(10_000, c.bytes_ref, &c);
        assert!(fast > slow, "higher rate must score higher ({fast} vs {slow})");
        // Touching huge volume very slowly does not max out the score.
        let slow_huge = throughput_score(10_000_000, c.bytes_ref * 100, &c);
        assert!(slow_huge < fast);
    }
}
