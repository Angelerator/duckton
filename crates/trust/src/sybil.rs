//! Sybil resistance (architecture §7.1): costly identity minting via proof-of-work
//! and a web-of-trust vouching scheme.
//!
//! * **PoW minting** — a new `node_id` must accompany a nonce such that
//!   `BLAKE3(domain ‖ pubkey ‖ nonce)` has at least `difficulty_bits` leading
//!   zero bits. This raises the cost of mass-producing identities.
//! * **Vouching** — an existing trusted node signs a `Vouch{subject, weight}`,
//!   lending bootstrap trust to a newcomer.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use p2p_proto::NodeId;
use serde::{Deserialize, Serialize};

const POW_DOMAIN: &[u8] = b"duckdb-p2p-identity-pow-v1";
const VOUCH_DOMAIN: &[u8] = b"duckdb-p2p-vouch-v1";

/// Count leading zero bits across a byte slice.
fn leading_zero_bits(bytes: &[u8]) -> u32 {
    let mut count = 0;
    for &b in bytes {
        if b == 0 {
            count += 8;
        } else {
            count += b.leading_zeros();
            break;
        }
    }
    count
}

/// Compute the PoW digest for a public key + nonce.
fn pow_digest(pubkey: &[u8; 32], nonce: u64) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(POW_DOMAIN);
    hasher.update(pubkey);
    hasher.update(&nonce.to_le_bytes());
    *hasher.finalize().as_bytes()
}

/// A proof-of-work stamp binding a nonce to a public key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowStamp {
    pub nonce: u64,
    pub difficulty_bits: u32,
}

/// Mint an identity stamp by searching for a qualifying nonce.
///
/// Returns `None` if no nonce is found within `max_iters` (only relevant for
/// pathological difficulties; callers normally loop until success).
pub fn mint_pow(pubkey: &[u8; 32], difficulty_bits: u32, max_iters: u64) -> Option<PowStamp> {
    for nonce in 0..max_iters {
        if leading_zero_bits(&pow_digest(pubkey, nonce)) >= difficulty_bits {
            return Some(PowStamp {
                nonce,
                difficulty_bits,
            });
        }
    }
    None
}

/// Verify a PoW stamp meets at least `required_bits`.
pub fn verify_pow(pubkey: &[u8; 32], stamp: &PowStamp, required_bits: u32) -> bool {
    if stamp.difficulty_bits < required_bits {
        return false;
    }
    leading_zero_bits(&pow_digest(pubkey, stamp.nonce)) >= required_bits
}

/// A signed vouch lending trust to a subject node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Vouch {
    pub voucher_pubkey: String, // hex ed25519
    pub subject: NodeId,
    /// Trust weight in `[0,1]` the voucher assigns.
    pub weight_milli: u32, // weight * 1000, to stay serde-stable
    pub sig: String, // hex ed25519
}

fn vouch_signing_bytes(voucher_pubkey: &[u8; 32], subject: &NodeId, weight_milli: u32) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(VOUCH_DOMAIN);
    buf.extend_from_slice(voucher_pubkey);
    buf.extend_from_slice(subject.0.as_bytes());
    buf.extend_from_slice(&weight_milli.to_le_bytes());
    buf
}

/// Create a signed vouch (`weight` clamped to `[0,1]`).
pub fn make_vouch(signing_key: &SigningKey, subject: &NodeId, weight: f64) -> Vouch {
    let pubkey = signing_key.verifying_key().to_bytes();
    let weight_milli = (weight.clamp(0.0, 1.0) * 1000.0).round() as u32;
    let msg = vouch_signing_bytes(&pubkey, subject, weight_milli);
    let sig = signing_key.sign(&msg);
    Vouch {
        voucher_pubkey: hex::encode(pubkey),
        subject: subject.clone(),
        weight_milli,
        sig: hex::encode(sig.to_bytes()),
    }
}

/// Verify a vouch's signature. Returns the weight in `[0,1]` if valid.
pub fn verify_vouch(vouch: &Vouch) -> Option<f64> {
    let pk_bytes = hex::decode(&vouch.voucher_pubkey).ok()?;
    let pk: [u8; 32] = pk_bytes.try_into().ok()?;
    let vk = VerifyingKey::from_bytes(&pk).ok()?;
    let sig_bytes = hex::decode(&vouch.sig).ok()?;
    let sig_arr: [u8; 64] = sig_bytes.try_into().ok()?;
    let sig = Signature::from_bytes(&sig_arr);
    let msg = vouch_signing_bytes(&pk, &vouch.subject, vouch.weight_milli);
    vk.verify(&msg, &sig).ok()?;
    Some(vouch.weight_milli as f64 / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;

    #[test]
    fn pow_mint_and_verify_small_difficulty() {
        let pubkey = [42u8; 32];
        let stamp = mint_pow(&pubkey, 12, 1_000_000).expect("should find nonce");
        assert!(verify_pow(&pubkey, &stamp, 12));
        // Higher requirement than minted should fail.
        assert!(!verify_pow(&pubkey, &stamp, 24) || stamp.difficulty_bits >= 24);
    }

    #[test]
    fn pow_rejects_wrong_pubkey() {
        let stamp = mint_pow(&[1u8; 32], 10, 1_000_000).unwrap();
        assert!(!verify_pow(&[2u8; 32], &stamp, 10));
    }

    #[test]
    fn pow_rejects_understated_difficulty() {
        let pubkey = [7u8; 32];
        let stamp = mint_pow(&pubkey, 8, 1_000_000).unwrap();
        // claims only 8 bits, but policy requires 16
        assert!(!verify_pow(&pubkey, &stamp, 16));
    }

    #[test]
    fn vouch_sign_and_verify() {
        let key = SigningKey::generate(&mut OsRng);
        let subject = NodeId("b3:newbie".into());
        let v = make_vouch(&key, &subject, 0.25);
        assert_eq!(verify_vouch(&v), Some(0.25));
    }

    #[test]
    fn tampered_vouch_rejected() {
        let key = SigningKey::generate(&mut OsRng);
        let mut v = make_vouch(&key, &NodeId("b3:x".into()), 0.5);
        v.weight_milli = 1000; // tamper to claim full trust
        assert_eq!(verify_vouch(&v), None);
    }
}
