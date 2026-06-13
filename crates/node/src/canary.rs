//! Node-level canary auditing helper (architecture §7.4 step 4).
//!
//! Wraps the trust crate's [`CanaryBook`] + injection decision so the
//! coordinator can recognize canary queries and judge results against a
//! known-good answer. The known answers are computed by a trusted engine and
//! registered here.

use std::sync::Mutex;

use p2p_proto::QueryHash;
use p2p_trust::canary::{self, CanaryBook};
use rand::rngs::StdRng;
use rand::SeedableRng;

/// Tracks known-answer canaries and decides when to inject one.
pub struct CanaryAuditor {
    book: CanaryBook,
    rate: f64,
    rng: Mutex<StdRng>,
}

impl CanaryAuditor {
    pub fn new(rate: f64, capacity: usize) -> Self {
        Self {
            book: CanaryBook::new(capacity),
            rate,
            rng: Mutex::new(StdRng::from_entropy()),
        }
    }

    /// Register a known-good answer for a query (its canonical result hash).
    pub fn register(&self, query: QueryHash, result_hash: String) {
        self.book.insert(query, result_hash);
    }

    /// The expected answer if `query` is a registered canary.
    pub fn expected(&self, query: &QueryHash) -> Option<String> {
        self.book.expected(query)
    }

    pub fn is_canary(&self, query: &QueryHash) -> bool {
        self.book.is_canary(query)
    }

    /// Whether the coordinator should inject a canary on this round.
    pub fn should_inject(&self) -> bool {
        let mut rng = self.rng.lock().unwrap();
        canary::should_inject_canary(self.rate, &mut *rng)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_expected() {
        let a = CanaryAuditor::new(0.0, 16);
        let q = QueryHash::compute("SELECT 1", "t");
        a.register(q.clone(), "h".into());
        assert!(a.is_canary(&q));
        assert_eq!(a.expected(&q).as_deref(), Some("h"));
        assert!(!a.should_inject()); // rate 0
    }
}
