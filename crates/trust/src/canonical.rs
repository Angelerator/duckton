//! Deterministic canonical result hashing + quorum agreement (architecture §7.4).
//!
//! DuckDB output is *not* byte-stable: parallel execution permutes row order and
//! numeric/NULL encodings vary. To make redundant execution comparable we:
//!  1. Encode each row to a canonical, type-tagged byte string.
//!  2. **Sort** the encoded rows (order-independence — handles parallel row order).
//!  3. Fold column names + row count + sorted rows into a single BLAKE3 hash.
//!
//! Same input + same SQL + same engine version ⇒ identical hash, regardless of
//! the order rows happen to be produced in.

use std::collections::HashMap;

use p2p_proto::{ResultSet, Value};

/// Domain-separation prefix so a result hash can never collide with another
/// BLAKE3 use in the system.
const DOMAIN: &[u8] = b"duckdb-p2p-result-v1";

/// Compute the canonical BLAKE3 hash of a result set, returned as lowercase hex.
pub fn canonical_hash(rs: &ResultSet) -> String {
    // Encode every row, then sort for order-independence.
    let mut encoded_rows: Vec<Vec<u8>> = rs.rows.iter().map(|row| encode_row(row)).collect();
    encoded_rows.sort_unstable();

    let mut hasher = blake3::Hasher::new();
    hasher.update(DOMAIN);
    // schema: column count + each name length-prefixed (column order is
    // deterministic in SQL, so we do NOT sort columns).
    update_len(&mut hasher, rs.columns.len() as u64);
    for col in &rs.columns {
        update_bytes(&mut hasher, col.as_bytes());
    }
    // rows
    update_len(&mut hasher, encoded_rows.len() as u64);
    for row in &encoded_rows {
        update_bytes(&mut hasher, row);
    }
    hex::encode(hasher.finalize().as_bytes())
}

/// Encode a single row to canonical bytes (type-tagged, length-prefixed cells).
fn encode_row(row: &[Value]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(row.len() * 8);
    buf.extend_from_slice(&(row.len() as u64).to_le_bytes());
    for v in row {
        encode_value(v, &mut buf);
    }
    buf
}

fn encode_value(v: &Value, buf: &mut Vec<u8>) {
    buf.push(v.type_tag());
    match v {
        Value::Null => {}
        Value::Bool(b) => buf.push(*b as u8),
        Value::Int(i) => buf.extend_from_slice(&i.to_le_bytes()),
        Value::Float(f) => buf.extend_from_slice(&canonical_f64_bits(*f).to_le_bytes()),
        Value::Text(s) => {
            buf.extend_from_slice(&(s.len() as u64).to_le_bytes());
            buf.extend_from_slice(s.as_bytes());
        }
        Value::Blob(b) => {
            buf.extend_from_slice(&(b.len() as u64).to_le_bytes());
            buf.extend_from_slice(b);
        }
    }
}

/// Normalize a float to canonical bits: collapse -0.0 → +0.0 and all NaNs to one
/// canonical NaN bit pattern, so equal numeric values hash equally.
fn canonical_f64_bits(f: f64) -> u64 {
    if f.is_nan() {
        0x7ff8_0000_0000_0000
    } else if f == 0.0 {
        0.0f64.to_bits() // both +0.0 and -0.0 map here
    } else {
        f.to_bits()
    }
}

fn update_len(hasher: &mut blake3::Hasher, n: u64) {
    hasher.update(&n.to_le_bytes());
}

fn update_bytes(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

/// The outcome of tallying committed result hashes (architecture §11 step 5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuorumOutcome {
    /// The hash that reached quorum, if any. `None` when no hash reached quorum
    /// **or** when the result is a [`split`](Self::split) (equivocation).
    pub agreed_hash: Option<String>,
    /// How many workers reported the most-agreed hash (deterministic: the highest
    /// count, ties broken by the lexicographically smallest hash).
    pub agreement: usize,
    /// The required quorum size.
    pub quorum: usize,
    /// `true` when **two or more distinct** hashes each independently reached
    /// quorum — a genuine equivocation/split. In that case `agreed_hash` is
    /// deliberately `None` (there is no single safe winner) and the caller should
    /// treat the attempt as inconclusive rather than silently picking a side.
    pub split: bool,
}

impl QuorumOutcome {
    pub fn reached(&self) -> bool {
        self.agreed_hash.is_some()
    }
}

/// Given each worker's committed result hash, determine whether any hash reached
/// quorum `q`.
///
/// Determinism: the tally is resolved by (count desc, hash asc) so two honest
/// verifiers given the same multiset always agree on the winner and the reported
/// `agreement` count — no reliance on `HashMap` iteration order. If more than one
/// distinct hash reaches quorum the outcome is flagged as a [`split`] with no
/// `agreed_hash` (equivocation is surfaced, not silently coin-flipped).
pub fn evaluate_quorum<'a, I>(hashes: I, quorum: usize) -> QuorumOutcome
where
    I: IntoIterator<Item = &'a str>,
{
    let mut tally: HashMap<&str, usize> = HashMap::new();
    for h in hashes {
        *tally.entry(h).or_insert(0) += 1;
    }
    // Deterministic ordering: highest count first, ties broken by smallest hash.
    let mut entries: Vec<(&str, usize)> = tally.into_iter().collect();
    entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));

    let meeting = entries.iter().filter(|(_, c)| *c >= quorum).count();
    let best_count = entries.first().map(|(_, c)| *c).unwrap_or(0);

    if meeting > 1 {
        // Equivocation: multiple distinct hashes each reached quorum.
        QuorumOutcome {
            agreed_hash: None,
            agreement: best_count,
            quorum,
            split: true,
        }
    } else if meeting == 1 {
        let (hash, count) = entries[0];
        QuorumOutcome {
            agreed_hash: Some(hash.to_string()),
            agreement: count,
            quorum,
            split: false,
        }
    } else {
        QuorumOutcome {
            agreed_hash: None,
            agreement: best_count,
            quorum,
            split: false,
        }
    }
}

/// One committed `(input_fingerprint, result_hash)` pair from a single provider,
/// for fingerprint-aware quorum (deterministic-input verification).
#[derive(Debug, Clone, Copy)]
pub struct CommitKey<'a> {
    /// The input snapshot the provider reported reading. Empty ⇒ unknown (an
    /// older worker, or a job with no pin) — treated as "on the pinned snapshot",
    /// never counted as drift, so an old peer can never trigger a false penalty.
    pub input_fingerprint: &'a str,
    /// The canonical result hash the provider committed.
    pub result_hash: &'a str,
}

/// The outcome of a fingerprint-aware quorum tally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FingerprintQuorumOutcome {
    /// Quorum over the result hashes of the commits that read the **pinned**
    /// snapshot (or all commits, when there is no pin). Drive the existing
    /// success/disagreement/verdict logic from this exactly as before.
    pub pinned: QuorumOutcome,
    /// How many commits read a **different**, non-empty snapshot than the pinned
    /// one — i.e. the source data changed between replica executions. `> 0` ⇒
    /// benign input drift: re-pin + re-dispatch, never a provider penalty.
    pub drifted: usize,
}

/// Tally committed result hashes **grouped by the input snapshot they read**
/// (deterministic-input verification, architecture §7.4 + §11 step 5).
///
/// Splits the commits into two groups against `pinned`:
///  * **on-pinned** — `input_fingerprint == pinned`, OR an *empty* fingerprint
///    (an older worker / unpinned job: treated as on-pinned so a missing
///    fingerprint is never a false drift), OR `pinned == None` (no pin at all:
///    every commit is on-pinned and this degrades to plain [`evaluate_quorum`]).
///  * **drifted** — a non-empty fingerprint that differs from `pinned`.
///
/// Quorum is then evaluated over the on-pinned group's result hashes only, so a
/// minority that read newer bytes (a *different* fingerprint) is NOT mixed into
/// the equivocation/disagreement check — it is surfaced as [`drifted`] instead.
/// A split with the SAME fingerprint but different hashes is still a genuine
/// equivocation (flagged by the inner [`QuorumOutcome::split`]); a split across
/// DIFFERENT fingerprints is drift.
pub fn evaluate_quorum_on_commits(
    commits: &[CommitKey<'_>],
    pinned: Option<&str>,
    quorum: usize,
) -> FingerprintQuorumOutcome {
    let mut on_pinned: Vec<&str> = Vec::with_capacity(commits.len());
    let mut drifted = 0usize;
    for c in commits {
        match pinned {
            Some(p) if !c.input_fingerprint.is_empty() && c.input_fingerprint != p => {
                drifted += 1;
            }
            _ => on_pinned.push(c.result_hash),
        }
    }
    FingerprintQuorumOutcome {
        pinned: evaluate_quorum(on_pinned, quorum),
        drifted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rs(rows: Vec<Vec<Value>>) -> ResultSet {
        ResultSet::new(vec!["a".into(), "b".into()], rows)
    }

    fn ck<'a>(fp: &'a str, h: &'a str) -> CommitKey<'a> {
        CommitKey {
            input_fingerprint: fp,
            result_hash: h,
        }
    }

    #[test]
    fn no_pin_degrades_to_plain_quorum() {
        let commits = [ck("", "h1"), ck("", "h1"), ck("", "h2")];
        let out = evaluate_quorum_on_commits(&commits, None, 2);
        assert_eq!(out.drifted, 0);
        assert_eq!(out.pinned.agreed_hash.as_deref(), Some("h1"));
    }

    #[test]
    fn minority_on_different_fingerprint_is_drift_not_disagreement() {
        // 2 read F0 and agree on h1; 1 read F1 (newer bytes) and got h2. With F0
        // pinned, the F1 commit is drift — not folded into the quorum tally.
        let commits = [ck("F0", "h1"), ck("F0", "h1"), ck("F1", "h2")];
        let out = evaluate_quorum_on_commits(&commits, Some("F0"), 2);
        assert_eq!(out.drifted, 1);
        assert_eq!(out.pinned.agreed_hash.as_deref(), Some("h1"));
        assert!(!out.pinned.split);
    }

    #[test]
    fn same_fingerprint_split_is_genuine_equivocation() {
        // Same pinned snapshot, two distinct hashes each reach quorum ⇒ split.
        let commits = [
            ck("F0", "h1"),
            ck("F0", "h1"),
            ck("F0", "h2"),
            ck("F0", "h2"),
        ];
        let out = evaluate_quorum_on_commits(&commits, Some("F0"), 2);
        assert_eq!(out.drifted, 0);
        assert!(out.pinned.split);
    }

    #[test]
    fn empty_fingerprint_is_not_drift() {
        // An older worker reports an empty fingerprint: never counted as drift.
        let commits = [ck("F0", "h1"), ck("", "h1")];
        let out = evaluate_quorum_on_commits(&commits, Some("F0"), 2);
        assert_eq!(out.drifted, 0);
        assert_eq!(out.pinned.agreement, 2);
    }

    #[test]
    fn row_order_does_not_affect_hash() {
        let a = rs(vec![
            vec![Value::Int(1), Value::Text("x".into())],
            vec![Value::Int(2), Value::Text("y".into())],
        ]);
        let b = rs(vec![
            vec![Value::Int(2), Value::Text("y".into())],
            vec![Value::Int(1), Value::Text("x".into())],
        ]);
        assert_eq!(canonical_hash(&a), canonical_hash(&b));
    }

    #[test]
    fn different_values_change_hash() {
        let a = rs(vec![vec![Value::Int(1), Value::Null]]);
        let b = rs(vec![vec![Value::Int(1), Value::Int(0)]]);
        assert_ne!(canonical_hash(&a), canonical_hash(&b));
    }

    #[test]
    fn int_and_text_do_not_collide() {
        let a = rs(vec![vec![Value::Int(1), Value::Null]]);
        let b = rs(vec![vec![Value::Text("1".into()), Value::Null]]);
        assert_ne!(canonical_hash(&a), canonical_hash(&b));
    }

    #[test]
    fn negative_zero_and_nan_normalized() {
        let a = rs(vec![vec![Value::Float(0.0), Value::Float(f64::NAN)]]);
        let b = rs(vec![vec![Value::Float(-0.0), Value::Float(f64::NAN)]]);
        assert_eq!(canonical_hash(&a), canonical_hash(&b));
    }

    #[test]
    fn column_names_matter() {
        let a = ResultSet::new(vec!["a".into()], vec![vec![Value::Int(1)]]);
        let b = ResultSet::new(vec!["z".into()], vec![vec![Value::Int(1)]]);
        assert_ne!(canonical_hash(&a), canonical_hash(&b));
    }

    #[test]
    fn quorum_reached_when_majority_agrees() {
        let out = evaluate_quorum(["h1", "h1", "h2"], 2);
        assert!(out.reached());
        assert_eq!(out.agreed_hash.as_deref(), Some("h1"));
        assert_eq!(out.agreement, 2);
    }

    #[test]
    fn quorum_not_reached_when_split() {
        let out = evaluate_quorum(["h1", "h2", "h3"], 2);
        assert!(!out.reached());
        assert!(
            !out.split,
            "no hash reached quorum, so it is not an equivocation"
        );
    }

    #[test]
    fn equivocation_is_flagged_as_split_not_silently_resolved() {
        // Two distinct hashes each reach quorum=2: must be surfaced as a split
        // with no agreed hash, never coin-flipped to one side.
        let out = evaluate_quorum(["h1", "h1", "h2", "h2"], 2);
        assert!(out.split);
        assert!(!out.reached());
        assert_eq!(out.agreed_hash, None);
    }

    #[test]
    fn quorum_is_deterministic_regardless_of_order() {
        let a = evaluate_quorum(["h1", "h1", "h2"], 2);
        let b = evaluate_quorum(["h2", "h1", "h1"], 2);
        assert_eq!(a, b);
        assert_eq!(a.agreed_hash.as_deref(), Some("h1"));
        assert!(!a.split);
    }
}
