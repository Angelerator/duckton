//! Resilient re-dispatch policy primitives (architecture §8/§11).
//!
//! Pure, deterministic building blocks the coordinator's re-dispatch loop uses:
//!  * [`Backoff`] — bounded exponential backoff with configurable jitter, so a
//!    swarm of requesters retrying a flaky host does not synchronize into a
//!    storm.
//!  * [`TokenBucket`] — a global retry/hedge budget: each re-dispatch past the
//!    first costs a token; an empty bucket stops the loop (storm guard).
//!  * [`FaultTally`] — fault attribution across an attempt's providers: keep
//!    retrying when nodes merely *die / time out* (transient), but STOP when a
//!    consensus of selected nodes fails the **same deterministic way** (the
//!    query is infeasible — a job fault, not a provider fault).
//!
//! Keeping these out of the async coordinator makes them trivially unit-testable
//! and keeps the loop itself readable.

use std::time::Duration;

use p2p_proto::Verdict;
use p2p_trust::is_job_consensus_failure;
use rand::Rng;

/// Bounded exponential backoff with jitter.
#[derive(Debug, Clone)]
pub struct Backoff {
    initial_ms: u64,
    max_ms: u64,
    jitter_frac: f64,
    attempt: u32,
}

impl Backoff {
    pub fn new(initial_ms: u64, max_ms: u64, jitter_frac: f64) -> Self {
        Self {
            initial_ms,
            max_ms: max_ms.max(initial_ms),
            jitter_frac: jitter_frac.clamp(0.0, 1.0),
            attempt: 0,
        }
    }

    /// The un-jittered base delay for the current attempt (`initial * 2^n`,
    /// capped at `max`).
    pub fn base_delay_ms(&self) -> u64 {
        let shift = self.attempt.min(63);
        let scaled = self.initial_ms.checked_shl(shift).unwrap_or(self.max_ms);
        scaled.min(self.max_ms)
    }

    /// Produce the next delay (with jitter applied) and advance the schedule.
    /// "Full jitter" at `jitter_frac = 1.0`: uniform in `[0, base]`; partial
    /// jitter keeps a floor: uniform in `[base*(1-frac), base]`.
    pub fn next_delay(&mut self) -> Duration {
        let base = self.base_delay_ms();
        self.attempt = self.attempt.saturating_add(1);
        if base == 0 {
            return Duration::ZERO;
        }
        let lo = ((base as f64) * (1.0 - self.jitter_frac)).round() as u64;
        let hi = base;
        let delay = if lo >= hi {
            hi
        } else {
            rand::thread_rng().gen_range(lo..=hi)
        };
        Duration::from_millis(delay)
    }

    /// Reset the schedule to the first attempt.
    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

/// A simple refilling token bucket used as the global retry/hedge budget.
///
/// `tokens` is a float so fractional refill works; `try_take` consumes one
/// whole token. A `capacity` of `0` means **unlimited** (the bucket is disabled
/// and always grants).
#[derive(Debug, Clone)]
pub struct TokenBucket {
    tokens: f64,
    capacity: f64,
    refill_per_sec: f64,
}

impl TokenBucket {
    /// Build a full bucket. `capacity == 0.0` ⇒ unlimited (always grants).
    pub fn new(capacity: f64, refill_per_sec: f64) -> Self {
        Self {
            tokens: capacity.max(0.0),
            capacity: capacity.max(0.0),
            refill_per_sec: refill_per_sec.max(0.0),
        }
    }

    /// Whether this bucket enforces a limit at all.
    pub fn is_unlimited(&self) -> bool {
        self.capacity == 0.0
    }

    /// Refill for `elapsed` since the last refill (no-op when unlimited).
    pub fn refill(&mut self, elapsed: Duration) {
        if self.is_unlimited() {
            return;
        }
        self.tokens = (self.tokens + self.refill_per_sec * elapsed.as_secs_f64()).min(self.capacity);
    }

    /// Try to consume one token. Always succeeds when unlimited.
    pub fn try_take(&mut self) -> bool {
        if self.is_unlimited() {
            return true;
        }
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Current token count (for tests / observability).
    pub fn available(&self) -> f64 {
        self.tokens
    }
}

/// Tally of how an attempt's selected providers failed, used to decide whether a
/// no-quorum outcome is a transient (retry) failure or a consensus-infeasible
/// **job** fault (stop).
#[derive(Debug, Default, Clone)]
pub struct FaultTally {
    /// Number of providers selected/dispatched this attempt.
    pub selected: usize,
    /// Per-fault-class counts (deterministic failures the providers reported).
    pub resource_exceeded: usize,
    pub infeasible: usize,
    /// Providers that simply went silent / timed out / abandoned (transient).
    pub silent: usize,
    /// Providers that committed a result (responded).
    pub committed: usize,
}

impl FaultTally {
    pub fn new(selected: usize) -> Self {
        Self {
            selected,
            ..Default::default()
        }
    }

    /// Record one provider's classified failure verdict.
    pub fn record(&mut self, verdict: Verdict) {
        match verdict {
            Verdict::ResourceExceeded => self.resource_exceeded += 1,
            Verdict::Infeasible => self.infeasible += 1,
            _ => self.silent += 1,
        }
    }

    pub fn record_committed(&mut self) {
        self.committed += 1;
    }

    pub fn record_silent(&mut self) {
        self.silent += 1;
    }

    /// Whether a consensus of selected providers failed the **same deterministic
    /// way** (all infeasible, or all resource-exceeded) so the failure is
    /// attributable to the **job**, not the providers — i.e. STOP re-dispatching.
    /// Transient silence/timeouts never count as consensus-infeasible.
    pub fn is_consensus_infeasible(&self, fraction: f64) -> bool {
        is_job_consensus_failure(self.infeasible, self.selected, fraction)
            || is_job_consensus_failure(self.resource_exceeded, self.selected, fraction)
    }

    /// A human-readable reason for the consensus-infeasible stop.
    pub fn consensus_reason(&self, fraction: f64) -> Option<String> {
        if is_job_consensus_failure(self.infeasible, self.selected, fraction) {
            Some(format!(
                "{}/{} selected providers reported the query infeasible (missing data / \
                 unsatisfiable) — job fault, not a provider fault",
                self.infeasible, self.selected
            ))
        } else if is_job_consensus_failure(self.resource_exceeded, self.selected, fraction) {
            Some(format!(
                "{}/{} selected providers exceeded resources running this query — the job is too \
                 expensive (job fault, not a provider fault)",
                self.resource_exceeded, self.selected
            ))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        let mut b = Backoff::new(100, 1_000, 0.0);
        assert_eq!(b.base_delay_ms(), 100);
        assert_eq!(b.next_delay(), Duration::from_millis(100));
        assert_eq!(b.base_delay_ms(), 200);
        b.next_delay();
        assert_eq!(b.base_delay_ms(), 400);
        b.next_delay();
        assert_eq!(b.base_delay_ms(), 800);
        b.next_delay();
        // capped at max
        assert_eq!(b.base_delay_ms(), 1_000);
    }

    #[test]
    fn backoff_jitter_stays_within_bounds() {
        let mut b = Backoff::new(1_000, 1_000, 0.5);
        for _ in 0..50 {
            let d = b.next_delay().as_millis() as u64;
            assert!((500..=1_000).contains(&d), "jittered delay {d} out of [500,1000]");
            b.reset();
        }
    }

    #[test]
    fn token_bucket_caps_a_storm_then_refills() {
        let mut tb = TokenBucket::new(3.0, 1.0);
        assert!(tb.try_take());
        assert!(tb.try_take());
        assert!(tb.try_take());
        assert!(!tb.try_take(), "bucket should be empty after 3 takes");
        tb.refill(Duration::from_secs(2));
        assert!(tb.try_take());
        assert!(tb.try_take());
        assert!(!tb.try_take());
    }

    #[test]
    fn token_bucket_zero_capacity_is_unlimited() {
        let mut tb = TokenBucket::new(0.0, 0.0);
        assert!(tb.is_unlimited());
        for _ in 0..1000 {
            assert!(tb.try_take());
        }
    }

    #[test]
    fn fault_tally_consensus_infeasible_only_on_same_deterministic_failure() {
        // 3 providers, all infeasible → consensus-infeasible (stop).
        let mut t = FaultTally::new(3);
        t.record(Verdict::Infeasible);
        t.record(Verdict::Infeasible);
        t.record(Verdict::Infeasible);
        assert!(t.is_consensus_infeasible(0.67));
        assert!(t.consensus_reason(0.67).is_some());

        // 3 providers, all silent (timeout) → transient, NOT consensus (retry).
        let mut t2 = FaultTally::new(3);
        t2.record_silent();
        t2.record_silent();
        t2.record_silent();
        assert!(!t2.is_consensus_infeasible(0.67));

        // Mixed: 1 infeasible + 2 silent → not consensus.
        let mut t3 = FaultTally::new(3);
        t3.record(Verdict::Infeasible);
        t3.record_silent();
        t3.record_silent();
        assert!(!t3.is_consensus_infeasible(0.67));
    }
}
