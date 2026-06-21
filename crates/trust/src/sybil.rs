//! Sybil resistance (architecture §7.1): costly identity minting via proof-of-work
//! and a web-of-trust vouching scheme.
//!
//! * **PoW minting** — a new `node_id` must accompany a nonce such that
//!   `BLAKE3(domain ‖ pubkey ‖ epoch ‖ nonce)` has at least `difficulty_bits`
//!   leading zero bits. Binding a coarse time **epoch** into the preimage means a
//!   single solved nonce is only valid for ~one day, so a Sybil fleet must
//!   re-mint every epoch — raising the *ongoing* (not one-time) cost.
//! * **Vouching** — an existing trusted node signs a `Vouch{subject, weight}`,
//!   lending bootstrap trust to a newcomer.

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use p2p_proto::NodeId;
use serde::{Deserialize, Serialize};

const POW_DOMAIN: &[u8] = b"duckdb-p2p-identity-pow-v2";
// v2 binds an `expiry` into the signed vouch (v1 vouches never expired, so a
// captured vouch was replayable forever). The domain bump prevents a v1
// signature from being reinterpreted as a v2 one.
const VOUCH_DOMAIN: &[u8] = b"duckdb-p2p-vouch-v2";

/// Length of a PoW epoch in seconds (1 day). The PoW preimage binds the epoch a
/// capability ad's `ts` falls in, so a solved nonce expires after ~one day and
/// mass identities must re-mint each epoch.
pub const POW_EPOCH_SECS: u64 = 24 * 3600;

/// The PoW epoch a unix-seconds timestamp falls in.
pub fn pow_epoch(ts: u64) -> u64 {
    ts / POW_EPOCH_SECS
}

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

/// Compute the PoW digest for a public key bound to a time epoch + nonce.
fn pow_digest(pubkey: &[u8; 32], epoch: u64, nonce: u64) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(POW_DOMAIN);
    hasher.update(pubkey);
    hasher.update(&epoch.to_le_bytes());
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
pub fn mint_pow(
    pubkey: &[u8; 32],
    epoch: u64,
    difficulty_bits: u32,
    max_iters: u64,
) -> Option<PowStamp> {
    for nonce in 0..max_iters {
        if leading_zero_bits(&pow_digest(pubkey, epoch, nonce)) >= difficulty_bits {
            return Some(PowStamp {
                nonce,
                difficulty_bits,
            });
        }
    }
    None
}

/// Verify a PoW stamp meets at least `required_bits` for the given `epoch`.
pub fn verify_pow(pubkey: &[u8; 32], epoch: u64, stamp: &PowStamp, required_bits: u32) -> bool {
    if stamp.difficulty_bits < required_bits {
        return false;
    }
    leading_zero_bits(&pow_digest(pubkey, epoch, stamp.nonce)) >= required_bits
}

/// A signed vouch lending trust to a subject node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Vouch {
    pub voucher_pubkey: String, // hex ed25519
    pub subject: NodeId,
    /// Trust weight in `[0,1]` the voucher assigns.
    pub weight_milli: u32, // weight * 1000, to stay serde-stable
    /// Unix-seconds expiry: a vouch is only valid up to this time. Part of the
    /// signed payload so it cannot be extended; `0` is treated as already-expired
    /// so an issuer cannot mint a never-expiring (forever-replayable) vouch.
    pub expiry: u64,
    pub sig: String, // hex ed25519
}

fn vouch_signing_bytes(
    voucher_pubkey: &[u8; 32],
    subject: &NodeId,
    weight_milli: u32,
    expiry: u64,
) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(VOUCH_DOMAIN);
    buf.extend_from_slice(voucher_pubkey);
    buf.extend_from_slice(subject.0.as_bytes());
    buf.extend_from_slice(&weight_milli.to_le_bytes());
    buf.extend_from_slice(&expiry.to_le_bytes());
    buf
}

/// Create a signed vouch (`weight` clamped to `[0,1]`) valid until `expiry`
/// (unix seconds). The expiry is signed, bounding how long the vouch is usable.
pub fn make_vouch(signing_key: &SigningKey, subject: &NodeId, weight: f64, expiry: u64) -> Vouch {
    let pubkey = signing_key.verifying_key().to_bytes();
    let weight_milli = (weight.clamp(0.0, 1.0) * 1000.0).round() as u32;
    let msg = vouch_signing_bytes(&pubkey, subject, weight_milli, expiry);
    let sig = signing_key.sign(&msg);
    Vouch {
        voucher_pubkey: hex::encode(pubkey),
        subject: subject.clone(),
        weight_milli,
        expiry,
        sig: hex::encode(sig.to_bytes()),
    }
}

/// Verify a vouch's signature and freshness at `now` (unix seconds). Returns the
/// weight in `[0,1]` if the signature is valid AND the vouch has not expired. A
/// `0` (or past) expiry is rejected, so a captured vouch cannot be replayed
/// forever — the caller passes the current time.
pub fn verify_vouch(vouch: &Vouch, now: u64) -> Option<f64> {
    if vouch.expiry == 0 || now > vouch.expiry {
        return None;
    }
    let pk_bytes = hex::decode(&vouch.voucher_pubkey).ok()?;
    let pk: [u8; 32] = pk_bytes.try_into().ok()?;
    let vk = VerifyingKey::from_bytes(&pk).ok()?;
    let sig_bytes = hex::decode(&vouch.sig).ok()?;
    let sig_arr: [u8; 64] = sig_bytes.try_into().ok()?;
    let sig = Signature::from_bytes(&sig_arr);
    let msg = vouch_signing_bytes(&pk, &vouch.subject, vouch.weight_milli, vouch.expiry);
    vk.verify_strict(&msg, &sig).ok()?;
    Some(vouch.weight_milli as f64 / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;

    #[test]
    fn pow_mint_and_verify_small_difficulty() {
        let pubkey = [42u8; 32];
        let stamp = mint_pow(&pubkey, 0, 12, 1_000_000).expect("should find nonce");
        assert!(verify_pow(&pubkey, 0, &stamp, 12));
        // Higher requirement than minted should fail.
        assert!(!verify_pow(&pubkey, 0, &stamp, 24) || stamp.difficulty_bits >= 24);
    }

    #[test]
    fn pow_rejects_wrong_pubkey() {
        let stamp = mint_pow(&[1u8; 32], 0, 10, 1_000_000).unwrap();
        assert!(!verify_pow(&[2u8; 32], 0, &stamp, 10));
    }

    #[test]
    fn pow_rejects_understated_difficulty() {
        let pubkey = [7u8; 32];
        let stamp = mint_pow(&pubkey, 0, 8, 1_000_000).unwrap();
        // claims only 8 bits, but policy requires 16
        assert!(!verify_pow(&pubkey, 0, &stamp, 16));
    }

    #[test]
    fn pow_is_bound_to_epoch() {
        // A nonce solved for one epoch must NOT validate in a different epoch,
        // forcing periodic re-mint (the Sybil-cost fix).
        let pubkey = [9u8; 32];
        let stamp = mint_pow(&pubkey, 7, 12, 1_000_000).unwrap();
        assert!(verify_pow(&pubkey, 7, &stamp, 12));
        assert!(
            !verify_pow(&pubkey, 8, &stamp, 12),
            "stamp must not cross epochs"
        );
    }

    #[test]
    fn pow_epoch_buckets_by_day() {
        assert_eq!(pow_epoch(0), 0);
        assert_eq!(pow_epoch(POW_EPOCH_SECS - 1), 0);
        assert_eq!(pow_epoch(POW_EPOCH_SECS), 1);
    }

    #[test]
    fn vouch_sign_and_verify() {
        let key = SigningKey::generate(&mut OsRng);
        let subject = NodeId("b3:newbie".into());
        let v = make_vouch(&key, &subject, 0.25, 2_000_000_000);
        assert_eq!(verify_vouch(&v, 1_700_000_000), Some(0.25));
    }

    #[test]
    fn tampered_vouch_rejected() {
        let key = SigningKey::generate(&mut OsRng);
        let mut v = make_vouch(&key, &NodeId("b3:x".into()), 0.5, 2_000_000_000);
        v.weight_milli = 1000; // tamper to claim full trust
        assert_eq!(verify_vouch(&v, 1_700_000_000), None);
    }

    #[test]
    fn expired_vouch_rejected() {
        let key = SigningKey::generate(&mut OsRng);
        let v = make_vouch(&key, &NodeId("b3:x".into()), 0.5, 1000);
        // Valid before expiry, rejected after, and a 0-expiry never verifies.
        assert_eq!(verify_vouch(&v, 999), Some(0.5));
        assert_eq!(verify_vouch(&v, 2000), None);
        let forever = make_vouch(&key, &NodeId("b3:y".into()), 0.5, 0);
        assert_eq!(verify_vouch(&forever, 1), None);
    }

    #[test]
    fn extended_expiry_breaks_signature() {
        // The expiry is signed: bumping it to extend validity invalidates the sig.
        let key = SigningKey::generate(&mut OsRng);
        let mut v = make_vouch(&key, &NodeId("b3:x".into()), 0.5, 1000);
        v.expiry = u64::MAX;
        assert_eq!(verify_vouch(&v, 2000), None);
    }
}
