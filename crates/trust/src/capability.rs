//! Signing & verification of capability advertisements (architecture §8).
//!
//! The proto [`CapabilityAd`] carries the data; here we bind it to a node
//! identity with an Ed25519 signature and a Sybil-resistance PoW stamp.

use ed25519_dalek::{Signature, VerifyingKey};
use p2p_proto::{AttestationLevel, CapabilityAd, CapabilityProfile, NodeId};

use crate::receipt::Signer;
use crate::sybil::{pow_epoch, verify_pow, PowStamp};

const DOMAIN: &[u8] = b"duckdb-p2p-capability-ad-v1";
const PROFILE_DOMAIN: &[u8] = b"duckdb-p2p-capability-profile-v1";

/// Fields needed to build (and sign) a capability ad.
pub struct CapabilityDraft {
    pub addr: String,
    pub free_mem_bytes: u64,
    pub free_threads: u32,
    pub max_jobs: u32,
    pub attestation_level: AttestationLevel,
    pub price: u64,
    pub recent_receipts_root: Option<String>,
    pub pow: PowStamp,
    pub ts: u64,
}

fn signing_bytes(ad: &CapabilityAd) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut field = |b: &[u8]| {
        buf.extend_from_slice(&(b.len() as u64).to_le_bytes());
        buf.extend_from_slice(b);
    };
    field(DOMAIN);
    field(&ad.schema_version.to_le_bytes());
    field(ad.protocol_version.to_string().as_bytes());
    field(ad.node_id.0.as_bytes());
    field(ad.pubkey.as_bytes());
    field(ad.addr.as_bytes());
    field(&ad.free_mem_bytes.to_le_bytes());
    field(&ad.free_threads.to_le_bytes());
    field(&ad.max_jobs.to_le_bytes());
    field(&[ad.attestation_level as u8]);
    field(&ad.price.to_le_bytes());
    field(ad.recent_receipts_root.as_deref().unwrap_or("").as_bytes());
    field(&ad.pow_nonce.to_le_bytes());
    field(&ad.pow_bits.to_le_bytes());
    field(&ad.ts.to_le_bytes());
    buf
}

/// Build and sign a capability ad with the node identity.
pub fn sign_capability_ad(draft: CapabilityDraft, signer: &impl Signer) -> CapabilityAd {
    let pubkey = signer.public_key();
    let mut ad = CapabilityAd {
        schema_version: p2p_proto::SCHEMA_VERSION,
        protocol_version: p2p_proto::PROTOCOL_VERSION,
        node_id: signer.node_id(),
        pubkey: hex::encode(pubkey),
        addr: draft.addr,
        free_mem_bytes: draft.free_mem_bytes,
        free_threads: draft.free_threads,
        max_jobs: draft.max_jobs,
        attestation_level: draft.attestation_level,
        price: draft.price,
        recent_receipts_root: draft.recent_receipts_root,
        pow_nonce: draft.pow.nonce,
        pow_bits: draft.pow.difficulty_bits,
        ts: draft.ts,
        sig: String::new(),
    };
    let sig = signer.sign_bytes(&signing_bytes(&ad));
    ad.sig = hex::encode(sig);
    ad
}

/// Verify a capability ad: signature, node-id binding, and PoW difficulty.
pub fn verify_capability_ad(ad: &CapabilityAd, required_pow_bits: u32) -> bool {
    // pubkey -> node_id binding
    let pk_bytes = match hex::decode(&ad.pubkey) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let pk: [u8; 32] = match pk_bytes.try_into() {
        Ok(a) => a,
        Err(_) => return false,
    };
    if ad.node_id != NodeId::from_pubkey(&pk) {
        return false;
    }
    // PoW — bound to the epoch the ad's timestamp falls in, so a single solved
    // nonce can't be reused across epochs to mint unlimited fresh-looking ads.
    let stamp = PowStamp {
        nonce: ad.pow_nonce,
        difficulty_bits: ad.pow_bits,
    };
    if !verify_pow(&pk, pow_epoch(ad.ts), &stamp, required_pow_bits) {
        return false;
    }
    // signature
    let vk = match VerifyingKey::from_bytes(&pk) {
        Ok(k) => k,
        Err(_) => return false,
    };
    let sig_bytes = match hex::decode(&ad.sig) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let sig_arr: [u8; 64] = match sig_bytes.try_into() {
        Ok(a) => a,
        Err(_) => return false,
    };
    vk.verify_strict(&signing_bytes(ad), &Signature::from_bytes(&sig_arr))
        .is_ok()
}

// ---------------------------------------------------------------------------
// Durable capability profile (architecture §3): self-measured, signed, monotonic
// ---------------------------------------------------------------------------

/// The mutable maxima/counters of a [`CapabilityProfile`] (everything except the
/// identity + signature, which `sign_capability_profile` fills in).
#[derive(Debug, Clone, Copy, Default)]
pub struct CapabilityProfileDraft {
    pub max_input_bytes: u64,
    pub max_result_rows: u64,
    pub max_result_bytes: u64,
    pub max_peak_memory_bytes: u64,
    pub max_temp_dir_bytes: u64,
    pub successes: u64,
    pub seq: u64,
    pub ts: u64,
}

fn profile_signing_bytes(p: &CapabilityProfile) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut field = |b: &[u8]| {
        buf.extend_from_slice(&(b.len() as u64).to_le_bytes());
        buf.extend_from_slice(b);
    };
    field(PROFILE_DOMAIN);
    field(&p.schema_version.to_le_bytes());
    field(p.node_id.0.as_bytes());
    field(p.pubkey.as_bytes());
    field(&p.max_input_bytes.to_le_bytes());
    field(&p.max_result_rows.to_le_bytes());
    field(&p.max_result_bytes.to_le_bytes());
    field(&p.max_peak_memory_bytes.to_le_bytes());
    field(&p.max_temp_dir_bytes.to_le_bytes());
    field(&p.successes.to_le_bytes());
    field(&p.seq.to_le_bytes());
    field(&p.ts.to_le_bytes());
    buf
}

/// Build and sign a capability profile with the node identity.
pub fn sign_capability_profile(
    draft: CapabilityProfileDraft,
    signer: &impl Signer,
) -> CapabilityProfile {
    let pubkey = signer.public_key();
    let mut p = CapabilityProfile {
        schema_version: p2p_proto::SCHEMA_VERSION,
        node_id: signer.node_id(),
        pubkey: hex::encode(pubkey),
        max_input_bytes: draft.max_input_bytes,
        max_result_rows: draft.max_result_rows,
        max_result_bytes: draft.max_result_bytes,
        max_peak_memory_bytes: draft.max_peak_memory_bytes,
        max_temp_dir_bytes: draft.max_temp_dir_bytes,
        successes: draft.successes,
        seq: draft.seq,
        ts: draft.ts,
        sig: String::new(),
    };
    let sig = signer.sign_bytes(&profile_signing_bytes(&p));
    p.sig = hex::encode(sig);
    p
}

/// Verify a capability profile: signature + node-id↔pubkey binding. Returns
/// `false` for a tampered field, a wrong-node copy, or a malformed signature.
pub fn verify_capability_profile(p: &CapabilityProfile) -> bool {
    let pk_bytes = match hex::decode(&p.pubkey) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let pk: [u8; 32] = match pk_bytes.try_into() {
        Ok(a) => a,
        Err(_) => return false,
    };
    if p.node_id != NodeId::from_pubkey(&pk) {
        return false;
    }
    let vk = match VerifyingKey::from_bytes(&pk) {
        Ok(k) => k,
        Err(_) => return false,
    };
    let sig_bytes = match hex::decode(&p.sig) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let sig_arr: [u8; 64] = match sig_bytes.try_into() {
        Ok(a) => a,
        Err(_) => return false,
    };
    vk.verify_strict(&profile_signing_bytes(p), &Signature::from_bytes(&sig_arr))
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sybil::{mint_pow, pow_epoch};
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

    fn draft(pubkey: &[u8; 32]) -> CapabilityDraft {
        CapabilityDraft {
            addr: "127.0.0.1:9494".into(),
            free_mem_bytes: 1 << 30,
            free_threads: 4,
            max_jobs: 3,
            attestation_level: AttestationLevel::L0,
            price: 0,
            recent_receipts_root: None,
            // PoW must be minted for the same epoch the ad's `ts` falls in.
            pow: mint_pow(pubkey, pow_epoch(100), 12, 1_000_000).unwrap(),
            ts: 100,
        }
    }

    #[test]
    fn sign_and_verify_capability_ad() {
        let key = SigningKey::generate(&mut OsRng);
        let signer = TestSigner(key.clone());
        let pk = key.verifying_key().to_bytes();
        let ad = sign_capability_ad(draft(&pk), &signer);
        assert!(verify_capability_ad(&ad, 12));
    }

    #[test]
    fn tampered_ad_rejected() {
        let key = SigningKey::generate(&mut OsRng);
        let signer = TestSigner(key.clone());
        let pk = key.verifying_key().to_bytes();
        let mut ad = sign_capability_ad(draft(&pk), &signer);
        ad.free_mem_bytes = u64::MAX; // lie about capacity
        assert!(!verify_capability_ad(&ad, 12));
    }

    #[test]
    fn insufficient_pow_rejected() {
        let key = SigningKey::generate(&mut OsRng);
        let signer = TestSigner(key.clone());
        let pk = key.verifying_key().to_bytes();
        let ad = sign_capability_ad(draft(&pk), &signer);
        // require more PoW than was minted
        assert!(!verify_capability_ad(&ad, 24));
    }

    #[test]
    fn sign_and_verify_capability_profile() {
        let signer = TestSigner(SigningKey::generate(&mut OsRng));
        let p = sign_capability_profile(
            CapabilityProfileDraft {
                max_input_bytes: 1 << 30,
                max_result_rows: 10_000,
                max_result_bytes: 4 << 20,
                successes: 7,
                seq: 3,
                ts: 1000,
                ..Default::default()
            },
            &signer,
        );
        assert!(verify_capability_profile(&p));
    }

    #[test]
    fn tampered_profile_rejected() {
        let signer = TestSigner(SigningKey::generate(&mut OsRng));
        let mut p = sign_capability_profile(
            CapabilityProfileDraft {
                max_result_rows: 10,
                seq: 1,
                ..Default::default()
            },
            &signer,
        );
        p.max_result_rows = u64::MAX; // inflate after signing
        assert!(!verify_capability_profile(&p));
    }

    #[test]
    fn profile_from_another_node_rejected() {
        let a = TestSigner(SigningKey::generate(&mut OsRng));
        let b = TestSigner(SigningKey::generate(&mut OsRng));
        let mut p = sign_capability_profile(CapabilityProfileDraft::default(), &a);
        // Re-label it as B's profile (without B's signature) — binding check fails.
        p.node_id = b.node_id();
        assert!(!verify_capability_profile(&p));
    }
}
