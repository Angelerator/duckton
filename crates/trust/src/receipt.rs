//! Signed receipts (architecture §7.3).
//!
//! A receipt is an Ed25519-signed statement by the requester about a completed
//! job. Verifiers (and the gossip layer) can check the signature offline. The
//! `Receipt` data type lives in `p2p-proto`; this module provides the canonical
//! signing-byte derivation plus sign/verify.

use ed25519_dalek::{Signature, VerifyingKey};
use p2p_proto::{JobId, NodeId, QueryHash, Receipt, Verdict};

/// Abstraction over "something that can sign with the node identity key", so the
/// trust crate need not depend on the transport crate. Implemented for the
/// transport's `NodeIdentity` in the node layer.
pub trait Signer {
    fn sign_bytes(&self, msg: &[u8]) -> [u8; 64];
    fn public_key(&self) -> [u8; 32];
    fn node_id(&self) -> NodeId;
}

/// Fields needed to build a receipt before signing.
#[derive(Debug, Clone)]
pub struct ReceiptDraft {
    pub job_id: JobId,
    pub worker_id: NodeId,
    pub query_hash: QueryHash,
    pub result_hash: String,
    pub verdict: Verdict,
    pub latency_ms: u64,
    pub ts: u64,
}

/// Canonical bytes that a receipt's signature covers. Stable field order with
/// length-prefixing so distinct field values can never produce the same bytes.
pub fn signing_bytes(
    job_id: &JobId,
    worker_id: &NodeId,
    requester_id: &NodeId,
    query_hash: &QueryHash,
    result_hash: &str,
    verdict: Verdict,
    latency_ms: u64,
    ts: u64,
) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"duckdb-p2p-receipt-v1");
    let mut field = |b: &[u8]| {
        buf.extend_from_slice(&(b.len() as u64).to_le_bytes());
        buf.extend_from_slice(b);
    };
    field(job_id.0.as_bytes());
    field(worker_id.0.as_bytes());
    field(requester_id.0.as_bytes());
    field(query_hash.0.as_bytes());
    field(result_hash.as_bytes());
    buf.push(verdict_tag(verdict));
    buf.extend_from_slice(&latency_ms.to_le_bytes());
    buf.extend_from_slice(&ts.to_le_bytes());
    buf
}

fn verdict_tag(v: Verdict) -> u8 {
    match v {
        Verdict::Correct => 1,
        Verdict::Incorrect => 2,
        Verdict::Timeout => 3,
        Verdict::Malformed => 4,
        Verdict::ResourceExceeded => 5,
        Verdict::Infeasible => 6,
        Verdict::Inconclusive => 7,
    }
}

/// Sign a receipt draft with the requester's identity.
pub fn sign_receipt(draft: ReceiptDraft, signer: &impl Signer) -> Receipt {
    let requester_id = signer.node_id();
    let msg = signing_bytes(
        &draft.job_id,
        &draft.worker_id,
        &requester_id,
        &draft.query_hash,
        &draft.result_hash,
        draft.verdict,
        draft.latency_ms,
        draft.ts,
    );
    let sig = signer.sign_bytes(&msg);
    Receipt {
        job_id: draft.job_id,
        worker_id: draft.worker_id,
        requester_id,
        query_hash: draft.query_hash,
        result_hash: draft.result_hash,
        verdict: draft.verdict,
        latency_ms: draft.latency_ms,
        ts: draft.ts,
        requester_pubkey: hex::encode(signer.public_key()),
        sig: hex::encode(sig),
    }
}

/// Verify a receipt's Ed25519 signature against the embedded requester pubkey.
///
/// Note: this proves the receipt was issued by the holder of `requester_pubkey`.
/// Whether that requester is *trusted* is a separate, policy-level decision.
pub fn verify_receipt(r: &Receipt) -> bool {
    let pubkey_bytes = match hex::decode(&r.requester_pubkey) {
        Ok(b) if b.len() == 32 => b,
        _ => return false,
    };
    let mut pk = [0u8; 32];
    pk.copy_from_slice(&pubkey_bytes);
    let verifying_key = match VerifyingKey::from_bytes(&pk) {
        Ok(k) => k,
        Err(_) => return false,
    };
    // The requester_id must actually be the hash of the embedded pubkey,
    // otherwise a receipt could claim a different identity than it signed with.
    if r.requester_id != NodeId::from_pubkey(&pk) {
        return false;
    }
    let sig_bytes = match hex::decode(&r.sig) {
        Ok(b) if b.len() == 64 => b,
        _ => return false,
    };
    let mut sig = [0u8; 64];
    sig.copy_from_slice(&sig_bytes);
    let signature = Signature::from_bytes(&sig);

    let msg = signing_bytes(
        &r.job_id,
        &r.worker_id,
        &r.requester_id,
        &r.query_hash,
        &r.result_hash,
        r.verdict,
        r.latency_ms,
        r.ts,
    );
    // `verify_strict` (not `verify`) rejects signature malleability and
    // low-order / small-subgroup public keys, so a valid signature is unique and
    // cannot be mauled into a second distinct-but-valid `sig` (which would defeat
    // dedup-by-signature on gossiped receipts). See ed25519-dalek VerifyingKey.
    verifying_key.verify_strict(&msg, &signature).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer as _, SigningKey};
    use rand::rngs::OsRng;

    /// Minimal in-test signer wrapping a dalek key.
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

    fn draft() -> ReceiptDraft {
        ReceiptDraft {
            job_id: JobId::new(),
            worker_id: NodeId("b3:worker".into()),
            query_hash: QueryHash::compute("SELECT 1", "test"),
            result_hash: "abc".into(),
            verdict: Verdict::Correct,
            latency_ms: 12,
            ts: 1000,
        }
    }

    #[test]
    fn sign_then_verify_succeeds() {
        let signer = TestSigner(SigningKey::generate(&mut OsRng));
        let r = sign_receipt(draft(), &signer);
        assert!(verify_receipt(&r));
    }

    #[test]
    fn tampering_breaks_verification() {
        let signer = TestSigner(SigningKey::generate(&mut OsRng));
        let mut r = sign_receipt(draft(), &signer);
        r.verdict = Verdict::Incorrect; // tamper after signing
        assert!(!verify_receipt(&r));
    }

    #[test]
    fn forged_requester_id_rejected() {
        let signer = TestSigner(SigningKey::generate(&mut OsRng));
        let mut r = sign_receipt(draft(), &signer);
        r.requester_id = NodeId("b3:someone-else".into());
        assert!(!verify_receipt(&r));
    }
}
