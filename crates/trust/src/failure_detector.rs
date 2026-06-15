//! Phi-accrual failure detector (architecture §8 liveness).
//!
//! Instead of a hard timeout, the φ-accrual detector ([Hayashibara et al., 2004])
//! outputs a continuously rising **suspicion level** `φ = -log10(P_late)` from
//! the distribution of recent heartbeat inter-arrival times. A peer is convicted
//! (suspected dead) once `φ` crosses a configurable threshold (~8–12). This
//! adapts to a link's natural jitter: a normally-bursty peer is not falsely
//! convicted, while a peer that has clearly gone quiet relative to its own
//! history is flagged quickly.
//!
//! Pure and transport-agnostic: callers feed it heartbeat arrival timestamps
//! (from the gossip/heartbeat layer) and query `phi(now)`. The node layer wraps
//! this per-peer and combines it with SWIM indirect probing before excluding a
//! peer from candidate selection.

use std::collections::VecDeque;

use p2p_config::PhiAccrualConfig;

/// A sliding-window phi-accrual detector for a single peer's heartbeats.
#[derive(Debug, Clone)]
pub struct PhiDetector {
    /// Recent inter-arrival intervals (ms), bounded to `window_size`.
    intervals: VecDeque<f64>,
    /// Arrival timestamp (ms) of the most recent heartbeat.
    last_arrival_ms: Option<u64>,
    window_size: usize,
    min_std_ms: f64,
    acceptable_pause_ms: f64,
    first_interval_ms: f64,
    convict_threshold: f64,
    /// Running sums for an O(1) mean/variance over the window.
    sum: f64,
    sum_sq: f64,
}

impl PhiDetector {
    /// Build from the phi config.
    pub fn new(cfg: &PhiAccrualConfig) -> Self {
        Self {
            intervals: VecDeque::with_capacity(cfg.window_size.max(1)),
            last_arrival_ms: None,
            window_size: cfg.window_size.max(1),
            min_std_ms: cfg.min_std_ms.max(f64::MIN_POSITIVE),
            acceptable_pause_ms: cfg.acceptable_pause_ms.max(0.0),
            first_interval_ms: cfg.first_interval_ms.max(f64::MIN_POSITIVE),
            convict_threshold: cfg.convict_threshold.max(f64::MIN_POSITIVE),
            sum: 0.0,
            sum_sq: 0.0,
        }
    }

    /// Record a heartbeat that arrived at `now_ms` (unix millis or any monotonic
    /// millisecond clock — only differences matter).
    pub fn heartbeat(&mut self, now_ms: u64) {
        if let Some(prev) = self.last_arrival_ms {
            // Ignore out-of-order / duplicate timestamps.
            if now_ms >= prev {
                let interval = (now_ms - prev) as f64;
                self.push_interval(interval);
            }
        }
        self.last_arrival_ms = Some(now_ms);
    }

    fn push_interval(&mut self, interval: f64) {
        self.intervals.push_back(interval);
        self.sum += interval;
        self.sum_sq += interval * interval;
        while self.intervals.len() > self.window_size {
            if let Some(old) = self.intervals.pop_front() {
                self.sum -= old;
                self.sum_sq -= old * old;
            }
        }
    }

    /// Number of recorded intervals (heartbeats - 1).
    pub fn sample_count(&self) -> usize {
        self.intervals.len()
    }

    /// Whether any heartbeat has been observed yet.
    pub fn has_heartbeat(&self) -> bool {
        self.last_arrival_ms.is_some()
    }

    /// Mean inter-arrival interval (ms), bootstrapped to `first_interval_ms`
    /// until at least one interval is observed.
    fn mean(&self) -> f64 {
        if self.intervals.is_empty() {
            self.first_interval_ms
        } else {
            self.sum / self.intervals.len() as f64
        }
    }

    /// Standard deviation (ms), floored at `min_std_ms`.
    fn std_dev(&self) -> f64 {
        if self.intervals.len() < 2 {
            return self.min_std_ms;
        }
        let n = self.intervals.len() as f64;
        let mean = self.sum / n;
        let var = (self.sum_sq / n - mean * mean).max(0.0);
        var.sqrt().max(self.min_std_ms)
    }

    /// Compute the suspicion level `φ` at `now_ms`. Higher = more suspicious.
    ///
    /// `φ = -log10(P(time-since-last-heartbeat))`, with the cumulative
    /// probability modeled by a logistic approximation of the normal CDF over
    /// the window's `(mean + acceptable_pause, std)`. Returns `0.0` before any
    /// heartbeat is seen (an unknown peer is not suspected).
    pub fn phi(&self, now_ms: u64) -> f64 {
        let last = match self.last_arrival_ms {
            Some(t) => t,
            None => return 0.0,
        };
        let elapsed = now_ms.saturating_sub(last) as f64;
        let mean = self.mean() + self.acceptable_pause_ms;
        let std = self.std_dev();
        // P(later than `elapsed`) = 1 - CDF(elapsed); φ = -log10 of that.
        let p_later = 1.0 - normal_cdf(elapsed, mean, std);
        // Clamp to avoid -inf / +inf; a vanishing tail probability ⇒ large φ.
        let p = p_later.clamp(1e-12, 1.0);
        -p.log10()
    }

    /// Whether the peer is currently **available** (not convicted): `φ` is below
    /// the configured conviction threshold.
    pub fn is_available(&self, now_ms: u64) -> bool {
        self.phi(now_ms) < self.convict_threshold
    }

    /// The configured conviction threshold (exposed for callers/tests).
    pub fn convict_threshold(&self) -> f64 {
        self.convict_threshold
    }
}

/// Logistic approximation of the standard-normal CDF (Akiyama), accurate to
/// ~1e-2 and monotonic — sufficient for a suspicion score and far cheaper than
/// `erf`. `cdf(x; mean, std) = 1 / (1 + exp(-1.702 * (x-mean)/std))`.
fn normal_cdf(x: f64, mean: f64, std: f64) -> f64 {
    let z = (x - mean) / std.max(f64::MIN_POSITIVE);
    1.0 / (1.0 + (-1.702 * z).exp())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> PhiAccrualConfig {
        PhiAccrualConfig {
            enabled: true,
            convict_threshold: 8.0,
            window_size: 100,
            min_std_ms: 50.0,
            acceptable_pause_ms: 0.0,
            first_interval_ms: 1_000.0,
        }
    }

    #[test]
    fn unknown_peer_is_available() {
        let d = PhiDetector::new(&cfg());
        assert_eq!(d.phi(0), 0.0);
        assert!(d.is_available(10_000));
    }

    #[test]
    fn regular_heartbeats_keep_phi_low() {
        let mut d = PhiDetector::new(&cfg());
        // Steady 1000ms heartbeats.
        for i in 0..20u64 {
            d.heartbeat(i * 1_000);
        }
        let last = 19 * 1_000;
        // Just after the expected next beat: low suspicion.
        assert!(d.phi(last + 1_000) < 1.0, "phi={}", d.phi(last + 1_000));
        assert!(d.is_available(last + 1_000));
    }

    #[test]
    fn long_silence_convicts() {
        let mut d = PhiDetector::new(&cfg());
        for i in 0..20u64 {
            d.heartbeat(i * 1_000);
        }
        let last = 19 * 1_000;
        // φ rises monotonically with silence.
        let near = d.phi(last + 1_000);
        let far = d.phi(last + 30_000);
        assert!(far > near);
        // A very long pause relative to the 1s cadence convicts the peer.
        assert!(
            !d.is_available(last + 60_000),
            "phi={}",
            d.phi(last + 60_000)
        );
    }

    #[test]
    fn window_is_bounded() {
        let mut c = cfg();
        c.window_size = 8;
        let mut d = PhiDetector::new(&c);
        for i in 0..100u64 {
            d.heartbeat(i * 500);
        }
        assert!(d.sample_count() <= 8);
    }

    #[test]
    fn acceptable_pause_tolerates_a_gc_blip() {
        let mut base = cfg();
        base.acceptable_pause_ms = 0.0;
        let mut d0 = PhiDetector::new(&base);

        let mut tol = cfg();
        tol.acceptable_pause_ms = 5_000.0;
        let mut d1 = PhiDetector::new(&tol);

        for i in 0..20u64 {
            d0.heartbeat(i * 1_000);
            d1.heartbeat(i * 1_000);
        }
        let last = 19 * 1_000;
        // With a generous acceptable pause, the same gap yields lower suspicion.
        assert!(d1.phi(last + 4_000) < d0.phi(last + 4_000));
    }
}
