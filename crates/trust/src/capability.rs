//! Signing & verification of capability advertisements (architecture §8).
//!
//! The proto [`CapabilityAd`] carries the data; here we bind it to a node
//! identity with an Ed25519 signature and a Sybil-resistance PoW stamp.

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use p2p_proto::{AttestationLevel, CapabilityAd, NodeId};

use crate::receipt::Signer;
use crate::sybil::{verify_pow, PowStamp};

const DOMAIN: &[u8] = b"duckdb-p2p-capability-ad-v1";

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
    // PoW
    let stamp = PowStamp {
        nonce: ad.pow_nonce,
        difficulty_bits: ad.pow_bits,
    };
    if !verify_pow(&pk, &stamp, required_pow_bits) {
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
    vk.verify(&signing_bytes(ad), &Signature::from_bytes(&sig_arr))
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sybil::mint_pow;
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
            pow: mint_pow(pubkey, 12, 1_000_000).unwrap(),
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
}
