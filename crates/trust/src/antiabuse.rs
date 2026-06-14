//! Anti-abuse / robustness primitives (ARCHITECTURE "Abuse resistance").
//!
//! Pure, transport-agnostic building blocks used by the coordinator/worker:
//!  * **Failure fault attribution** — classify an execution/dispatch failure into
//!    a [`Verdict`] fault class, and decide (via a job-consensus signal) whether a
//!    no-quorum outcome should blame the providers or the job.
//!  * **Requester-trust weighting** — `w(requester) ∈ [0,1]` ("newer sender →
//!    less effect"), asymmetric so negative outcomes are gated hardest.
//!  * **Non-determinism detection** — flag queries that can't reach a stable
//!    quorum hash (`random()`, `now()`/`current_*`, unordered `LIMIT`, …).
//!  * **Signed abuse signals** — sign/verify the gossiped [`AbuseSignal`] so each
//!    node can independently refuse a flagged actor.

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use p2p_proto::{AbuseSignal, NodeId, Verdict};

use crate::reputation::age_factor;
use crate::receipt::Signer;

/// Classify a worker execution / dispatch error message into a fault class.
///
/// This is a deliberately conservative, heuristic mapping (string-based, since
/// DuckDB surfaces these as text): resource exhaustion ⇒ [`Verdict::ResourceExceeded`]
/// (job too expensive — provider blameless); a structurally infeasible query
/// (parse/binder/catalog/missing-data) ⇒ [`Verdict::Infeasible`] (requester
/// fault); anything else is left [`Verdict::Inconclusive`] (non-attributable —
/// we do **not** blame the provider for an error we cannot pin on it). A genuine
/// provable provider fault (wrong result vs. a verified quorum) is detected by
/// the quorum machinery, not here.
pub fn classify_failure(err_msg: &str) -> Verdict {
    let m = err_msg.to_ascii_lowercase();
    let resource = [
        "out of memory",
        "could not allocate",
        "memory limit",
        "failed to allocate",
        "exceeds the memory limit",
        "resource exhaust",
        "too large",
        "disk quota",
        "no space left",
    ];
    if resource.iter().any(|p| m.contains(p)) {
        return Verdict::ResourceExceeded;
    }
    let infeasible = [
        "syntax error",
        "parser error",
        "binder error",
        "catalog error",
        "does not exist",
        "no such",
        "not found",
        "referenced table",
        "referenced column",
        "type mismatch",
        "conversion error",
        "permission",
        "no files found",
    ];
    if infeasible.iter().any(|p| m.contains(p)) {
        return Verdict::Infeasible;
    }
    Verdict::Inconclusive
}

/// Job-consensus signal: given how many of the `selected` providers failed the
/// **same** way (e.g. all timed out / all OOM'd) and `fraction` ∈ [0,1], decide
/// whether the failure should be attributed to the **job** (true ⇒ no provider
/// penalty) rather than the providers. Returns true when the same-failure count
/// reaches `ceil(fraction * selected)` (and at least 2 providers were involved,
/// so a single straggler is never excused as "consensus").
pub fn is_job_consensus_failure(same_failure: usize, selected: usize, fraction: f64) -> bool {
    if selected < 2 {
        return false;
    }
    let need = (fraction.clamp(0.0, 1.0) * selected as f64).ceil() as usize;
    same_failure >= need.max(2)
}

/// Requester-trust weight `w ∈ [0,1]` applied to a job's effect on a provider's
/// score ("newer sender → less effect", ARCHITECTURE "Abuse resistance").
///
/// `w = floor + (1 - floor) · standing`, `standing = clamp01(age · reputation)`,
/// where `age = age_factor(observations, saturation)` and `reputation` is the
/// requester's own recency-weighted success rate (unknown ⇒ `0`). A brand-new
/// requester (`observations = 0`) gets exactly `floor`; an established requester
/// with good reputation approaches `1`.
///
/// Call with a **low** `floor` for negative outcomes (penalties) and a **higher**
/// `floor` for positive outcomes (reputation credit) to gate griefing hardest
/// while still letting honest new requesters give some positive credit.
pub fn requester_trust_weight(
    reputation: Option<f64>,
    observations: usize,
    age_saturation: usize,
    floor: f64,
) -> f64 {
    let floor = floor.clamp(0.0, 1.0);
    let age = age_factor(observations, age_saturation);
    let rep = reputation.unwrap_or(0.0).clamp(0.0, 1.0);
    let standing = (age * rep).clamp(0.0, 1.0);
    (floor + (1.0 - floor) * standing).clamp(0.0, 1.0)
}

/// Detect a query whose canonical result hash cannot be stable across redundant
/// providers, so it must be marked **non-verifiable** (no quorum, no provider
/// penalty for a "mismatch"). Heuristic, case-insensitive, comment/string-naive:
///  * non-deterministic functions: `random`, `now`, `current_date/time/timestamp/
///    localtime/localtimestamp`, `uuid`/`gen_random_uuid`, `nextval`, `txid`,
///    `current_setting`, `version()`, hostname/pid-style builtins;
///  * an unordered `LIMIT` (a `LIMIT` with no `ORDER BY` anywhere in the query),
///    whose returned rows are arbitrary.
///
/// It is intentionally conservative-toward-detection: a false positive only costs
/// a (correct) result being returned **non-verified** rather than penalizing a
/// provider, which is the safe direction.
pub fn is_nondeterministic(sql: &str) -> bool {
    let s = sql.to_ascii_lowercase();
    // Non-deterministic function calls (matched as `name(` to avoid hitting
    // identifiers/columns that merely contain the word).
    const FN_MARKERS: &[&str] = &[
        "random(",
        "now(",
        "uuid(",
        "gen_random_uuid(",
        "nextval(",
        "txid(",
        "current_setting(",
        "version(",
        "random()",
    ];
    if FN_MARKERS.iter().any(|m| s.contains(m)) {
        return true;
    }
    // `current_*` time/date builtins are often used without parentheses.
    const CURRENT_MARKERS: &[&str] = &[
        "current_date",
        "current_time",
        "current_timestamp",
        "current_localtime",
        "current_localtimestamp",
        "localtime",
        "localtimestamp",
        "get_current_time",
        "current_query",
    ];
    if CURRENT_MARKERS.iter().any(|m| s.contains(m)) {
        return true;
    }
    // Unordered LIMIT: a LIMIT clause with no ORDER BY ⇒ arbitrary row selection.
    if contains_word(&s, "limit") && !s.contains("order by") {
        return true;
    }
    false
}

/// Whether `s` contains `word` as a whitespace/paren/punct-delimited token.
fn contains_word(s: &str, word: &str) -> bool {
    let bytes = s.as_bytes();
    let w = word.as_bytes();
    let mut i = 0;
    while let Some(pos) = s[i..].find(word) {
        let start = i + pos;
        let end = start + w.len();
        let before_ok = start == 0 || !is_ident_byte(bytes[start - 1]);
        let after_ok = end >= bytes.len() || !is_ident_byte(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        i = start + 1;
        if i >= s.len() {
            break;
        }
    }
    false
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

// ---------------------------------------------------------------------------
// Signed abuse signals
// ---------------------------------------------------------------------------

/// Canonical signing bytes for an [`AbuseSignal`] (stable, length-prefixed).
fn abuse_signing_bytes(
    subject_id: &NodeId,
    subject_wallet: Option<&str>,
    reason: &str,
    ts: u64,
    reporter_id: &NodeId,
) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"duckdb-p2p-abuse-v1");
    let mut field = |b: &[u8]| {
        buf.extend_from_slice(&(b.len() as u64).to_le_bytes());
        buf.extend_from_slice(b);
    };
    field(subject_id.0.as_bytes());
    field(subject_wallet.unwrap_or("").as_bytes());
    field(reason.as_bytes());
    field(reporter_id.0.as_bytes());
    buf.extend_from_slice(&ts.to_le_bytes());
    buf
}

/// Sign an abuse signal with the reporter's node identity.
pub fn sign_abuse_signal(
    subject_id: NodeId,
    subject_wallet: Option<String>,
    reason: impl Into<String>,
    ts: u64,
    signer: &impl Signer,
) -> AbuseSignal {
    let reporter_id = signer.node_id();
    let reason = reason.into();
    let msg = abuse_signing_bytes(
        &subject_id,
        subject_wallet.as_deref(),
        &reason,
        ts,
        &reporter_id,
    );
    let sig = signer.sign_bytes(&msg);
    AbuseSignal {
        subject_id,
        subject_wallet,
        reason,
        ts,
        reporter_id,
        reporter_pubkey: hex::encode(signer.public_key()),
        sig: hex::encode(sig),
    }
}

/// Verify an abuse signal's Ed25519 signature against the embedded reporter
/// pubkey and that the `reporter_id` is the hash of that pubkey. Proves the
/// signal was issued by the holder of `reporter_pubkey`; whether to *act* on it
/// is a separate policy decision (`[antiabuse.blocklist].honor_gossip_signals`).
pub fn verify_abuse_signal(sig: &AbuseSignal) -> bool {
    let pubkey_bytes = match hex::decode(&sig.reporter_pubkey) {
        Ok(b) if b.len() == 32 => b,
        _ => return false,
    };
    let mut pk = [0u8; 32];
    pk.copy_from_slice(&pubkey_bytes);
    let verifying_key = match VerifyingKey::from_bytes(&pk) {
        Ok(k) => k,
        Err(_) => return false,
    };
    if sig.reporter_id != NodeId::from_pubkey(&pk) {
        return false;
    }
    let sig_bytes = match hex::decode(&sig.sig) {
        Ok(b) if b.len() == 64 => b,
        _ => return false,
    };
    let mut s = [0u8; 64];
    s.copy_from_slice(&sig_bytes);
    let signature = Signature::from_bytes(&s);
    let msg = abuse_signing_bytes(
        &sig.subject_id,
        sig.subject_wallet.as_deref(),
        &sig.reason,
        sig.ts,
        &sig.reporter_id,
    );
    verifying_key.verify(&msg, &signature).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer as _, SigningKey};
    use rand::rngs::OsRng;

    struct TestSigner(SigningKey);
    impl Signer for TestSigner {
        fn sign_bytes(&self, msg: &[u8]) -> [u8; 64] {
            self.0.sign(msg).to_bytes()
        }
        fn public_key(&self) -> [u8; 32] {
            self.0.verifying_key().to_bytes()
        }
        fn node_id(&self) -> NodeId {
            NodeId::from_pubkey(&self.0.verifying_key().to_bytes())
        }
    }

    #[test]
    fn classify_failure_maps_fault_classes() {
        assert_eq!(classify_failure("Out of Memory Error: failed to allocate 4GB"), Verdict::ResourceExceeded);
        assert_eq!(classify_failure("exceeds the memory limit of 1GB"), Verdict::ResourceExceeded);
        assert_eq!(classify_failure("Catalog Error: Table 'events' does not exist"), Verdict::Infeasible);
        assert_eq!(classify_failure("Parser Error: syntax error near SELECT"), Verdict::Infeasible);
        // Unknown errors are non-attributable, not provider fault.
        assert_eq!(classify_failure("weird transient blip"), Verdict::Inconclusive);
    }

    #[test]
    fn job_consensus_needs_majority_and_at_least_two() {
        // Single straggler is never "consensus".
        assert!(!is_job_consensus_failure(1, 1, 0.67));
        // 2 of 3 failing the same way at 0.67 ⇒ ceil(2.01)=3? 0.67*3=2.01 ceil=3 → needs 3.
        assert!(!is_job_consensus_failure(2, 3, 0.67));
        assert!(is_job_consensus_failure(3, 3, 0.67));
        // All of 2 failing the same way ⇒ consensus.
        assert!(is_job_consensus_failure(2, 2, 0.67));
    }

    #[test]
    fn requester_weight_gates_new_senders_and_grows_with_standing() {
        // New requester (0 obs): weight == floor.
        let w_new_neg = requester_trust_weight(None, 0, 50, 0.0);
        assert_eq!(w_new_neg, 0.0);
        let w_new_pos = requester_trust_weight(None, 0, 50, 0.5);
        assert_eq!(w_new_pos, 0.5);
        // Established, good-reputation requester approaches 1.0.
        let w_est = requester_trust_weight(Some(1.0), 50, 50, 0.0);
        assert!((w_est - 1.0).abs() < 1e-9);
        // Monotonic in observations at fixed reputation.
        let a = requester_trust_weight(Some(1.0), 5, 50, 0.0);
        let b = requester_trust_weight(Some(1.0), 25, 50, 0.0);
        assert!(b > a && a > 0.0);
        // Negative floor (0.0) gates a newcomer harder than the positive floor.
        assert!(requester_trust_weight(None, 0, 50, 0.0) < requester_trust_weight(None, 0, 50, 0.5));
    }

    #[test]
    fn nondeterminism_detection() {
        assert!(is_nondeterministic("SELECT random()"));
        assert!(is_nondeterministic("SELECT now() AS t"));
        assert!(is_nondeterministic("SELECT current_timestamp"));
        assert!(is_nondeterministic("SELECT gen_random_uuid()"));
        assert!(is_nondeterministic("SELECT * FROM t LIMIT 10"));
        // Deterministic queries are NOT flagged.
        assert!(!is_nondeterministic("SELECT region, count(*) FROM events GROUP BY region"));
        assert!(!is_nondeterministic("SELECT * FROM t ORDER BY id LIMIT 10"));
        assert!(!is_nondeterministic("SELECT 1"));
        // A column merely named like a keyword must not false-positive on word match.
        assert!(!is_nondeterministic("SELECT randomized_flag FROM t"));
    }

    #[test]
    fn abuse_signal_sign_then_verify() {
        let signer = TestSigner(SigningKey::generate(&mut OsRng));
        let sig = sign_abuse_signal(
            NodeId("b3:bad".into()),
            None,
            "equivocation",
            1234,
            &signer,
        );
        assert!(verify_abuse_signal(&sig));
    }

    #[test]
    fn tampered_abuse_signal_rejected() {
        let signer = TestSigner(SigningKey::generate(&mut OsRng));
        let mut sig = sign_abuse_signal(NodeId("b3:bad".into()), None, "slashed", 1, &signer);
        sig.reason = "nothing".into();
        assert!(!verify_abuse_signal(&sig));
        let mut sig2 = sign_abuse_signal(NodeId("b3:bad".into()), None, "slashed", 1, &signer);
        sig2.reporter_id = NodeId("b3:someone-else".into());
        assert!(!verify_abuse_signal(&sig2));
    }
}
