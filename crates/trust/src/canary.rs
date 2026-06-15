//! Canary auditing (architecture §7.4 step 4).
//!
//! The requester periodically injects a query whose canonical result hash it
//! already knows (computed locally or on a trusted node). A worker whose
//! committed hash doesn't match the known answer is marked `Incorrect` and
//! penalized. Canaries police even non-redundant jobs, cheaply and randomly.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use p2p_proto::{QueryHash, Verdict};
use rand::Rng;

/// Decide whether the next job should be a canary, given the configured rate.
pub fn should_inject_canary(rate: f64, rng: &mut impl Rng) -> bool {
    if rate <= 0.0 {
        return false;
    }
    if rate >= 1.0 {
        return true;
    }
    rng.gen::<f64>() < rate
}

/// Judge a worker's committed hash against a known-good answer.
pub fn judge_canary(expected_hash: &str, observed_hash: &str) -> Verdict {
    if expected_hash == observed_hash {
        Verdict::Correct
    } else {
        Verdict::Incorrect
    }
}

/// A bounded book of known-good answers (`query_hash -> canonical result hash`).
/// Used to recognize canary queries and judge results.
pub struct CanaryBook {
    inner: Mutex<Inner>,
    capacity: usize,
}

struct Inner {
    map: HashMap<QueryHash, String>,
    /// Insertion order for FIFO eviction (so the *oldest* canary is dropped, not
    /// an arbitrary one — keeps recently-registered canaries effective).
    order: VecDeque<QueryHash>,
}

impl CanaryBook {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                map: HashMap::new(),
                order: VecDeque::new(),
            }),
            capacity: capacity.max(1),
        }
    }

    /// Record a known answer for a query.
    pub fn insert(&self, query: QueryHash, result_hash: String) {
        let mut inner = self.inner.lock().unwrap();
        if !inner.map.contains_key(&query) {
            // evict oldest entries (FIFO) to stay bounded
            while inner.map.len() >= self.capacity {
                match inner.order.pop_front() {
                    Some(old) => {
                        inner.map.remove(&old);
                    }
                    None => break,
                }
            }
            inner.order.push_back(query.clone());
        }
        inner.map.insert(query, result_hash);
    }

    /// Look up the known answer for a query, if this is a canary.
    pub fn expected(&self, query: &QueryHash) -> Option<String> {
        self.inner.lock().unwrap().map.get(query).cloned()
    }

    pub fn is_canary(&self, query: &QueryHash) -> bool {
        self.inner.lock().unwrap().map.contains_key(query)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    #[test]
    fn rate_zero_never_injects_and_one_always() {
        let mut rng = StdRng::seed_from_u64(1);
        assert!(!should_inject_canary(0.0, &mut rng));
        assert!(should_inject_canary(1.0, &mut rng));
    }

    #[test]
    fn rate_is_approximately_honored() {
        let mut rng = StdRng::seed_from_u64(42);
        let n = 10_000;
        let hits = (0..n)
            .filter(|_| should_inject_canary(0.25, &mut rng))
            .count();
        let frac = hits as f64 / n as f64;
        assert!((frac - 0.25).abs() < 0.03, "got {frac}");
    }

    #[test]
    fn judge_matches_and_mismatches() {
        assert_eq!(judge_canary("h", "h"), Verdict::Correct);
        assert_eq!(judge_canary("h", "other"), Verdict::Incorrect);
    }

    #[test]
    fn canary_book_recognizes_and_is_bounded() {
        let book = CanaryBook::new(2);
        let q1 = QueryHash::compute("a", "t");
        book.insert(q1.clone(), "h1".into());
        assert!(book.is_canary(&q1));
        assert_eq!(book.expected(&q1).as_deref(), Some("h1"));
        book.insert(QueryHash::compute("b", "t"), "h2".into());
        book.insert(QueryHash::compute("c", "t"), "h3".into());
        // capacity 2 -> at most 2 retained; FIFO drops the oldest (q1)
        let inner = book.inner.lock().unwrap();
        assert!(inner.map.len() <= 2);
        assert!(!inner.map.contains_key(&q1), "oldest canary evicted first");
        assert_eq!(inner.order.len(), inner.map.len(), "order stays in sync");
    }
}
