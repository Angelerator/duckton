//! Sealed key transport for attestation-gated key release (architecture §9.3).
//!
//! A data key is sealed (encrypted) to a recipient's X25519 public key so only
//! the holder of the matching secret (e.g. the attested enclave) can open it.
//! Uses ephemeral-static X25519 ECDH → BLAKE3 KDF → ChaCha20-Poly1305 AEAD.
//!
//! This is real public-key sealing. In the confidential tier the recipient
//! secret lives only inside the TEE and its public half is bound into the
//! attestation quote, so the key is released *only* to a genuine enclave.

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce, XChaCha20Poly1305, XNonce};
use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use x25519_dalek::{EphemeralSecret, PublicKey, StaticSecret};

const KDF_DOMAIN: &[u8] = b"duckdb-p2p-seal-v1";

/// A recipient keypair able to open blobs sealed to its public key.
pub struct SealingKeypair {
    secret: StaticSecret,
    public: PublicKey,
}

impl SealingKeypair {
    pub fn generate() -> Self {
        let secret = StaticSecret::random_from_rng(OsRng);
        let public = PublicKey::from(&secret);
        Self { secret, public }
    }

    pub fn public_bytes(&self) -> [u8; 32] {
        self.public.to_bytes()
    }

    /// Open (decrypt) a blob sealed to this keypair's public key.
    pub fn open(&self, blob: &SealedBlob) -> Option<Vec<u8>> {
        let eph_pub = PublicKey::from(blob.ephemeral_pub);
        let shared = self.secret.diffie_hellman(&eph_pub);
        let key = derive_key(shared.as_bytes(), &blob.ephemeral_pub, &self.public_bytes());
        let cipher = ChaCha20Poly1305::new((&key).into());
        cipher
            .decrypt(Nonce::from_slice(&blob.nonce), blob.ciphertext.as_slice())
            .ok()
    }
}

/// A sealed ciphertext blob.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedBlob {
    pub ephemeral_pub: [u8; 32],
    pub nonce: [u8; 12],
    pub ciphertext: Vec<u8>,
}

impl SealedBlob {
    pub fn to_hex(&self) -> String {
        hex::encode(serde_json::to_vec(self).expect("sealed blob serializes"))
    }
    pub fn from_hex(s: &str) -> Option<Self> {
        let bytes = hex::decode(s).ok()?;
        serde_json::from_slice(&bytes).ok()
    }
}

/// Seal `plaintext` to a recipient X25519 public key.
pub fn seal_to(recipient_pub: &[u8; 32], plaintext: &[u8]) -> SealedBlob {
    let ephemeral = EphemeralSecret::random_from_rng(OsRng);
    let ephemeral_pub = PublicKey::from(&ephemeral).to_bytes();
    let shared = ephemeral.diffie_hellman(&PublicKey::from(*recipient_pub));
    let key = derive_key(shared.as_bytes(), &ephemeral_pub, recipient_pub);
    let cipher = ChaCha20Poly1305::new((&key).into());

    let mut nonce_bytes = [0u8; 12];
    OsRng.fill_bytes(&mut nonce_bytes);
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), plaintext)
        .expect("encryption never fails for valid key/nonce");

    SealedBlob {
        ephemeral_pub,
        nonce: nonce_bytes,
        ciphertext,
    }
}

/// Encrypt data at rest with a symmetric 32-byte data key (Parquet-Modular-
/// Encryption stand-in for local-file tests, architecture §9.2). Output is
/// `nonce(24) ‖ ciphertext`.
///
/// Uses **XChaCha20-Poly1305** (192-bit nonce): unlike the 96-bit
/// ChaCha20-Poly1305 nonce — which has a non-negligible collision probability
/// after ~2³² messages under a single key — a random 192-bit nonce is safe to
/// reuse across an effectively unbounded number of files under one long-lived
/// at-rest data key (no per-key message-count budget to track).
pub fn encrypt_at_rest(key: &[u8; 32], plaintext: &[u8]) -> Vec<u8> {
    let cipher = XChaCha20Poly1305::new(key.into());
    let mut nonce = [0u8; 24];
    OsRng.fill_bytes(&mut nonce);
    let ct = cipher
        .encrypt(XNonce::from_slice(&nonce), plaintext)
        .expect("encryption never fails for valid key/nonce");
    let mut out = Vec::with_capacity(24 + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    out
}

/// Decrypt data produced by [`encrypt_at_rest`]. Returns `None` if the key is
/// wrong or the ciphertext was tampered with.
pub fn decrypt_at_rest(key: &[u8; 32], blob: &[u8]) -> Option<Vec<u8>> {
    if blob.len() < 24 {
        return None;
    }
    let (nonce, ct) = blob.split_at(24);
    let cipher = XChaCha20Poly1305::new(key.into());
    cipher.decrypt(XNonce::from_slice(nonce), ct).ok()
}

fn derive_key(shared: &[u8; 32], ephemeral_pub: &[u8; 32], recipient_pub: &[u8; 32]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(KDF_DOMAIN);
    hasher.update(shared);
    hasher.update(ephemeral_pub);
    hasher.update(recipient_pub);
    *hasher.finalize().as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_roundtrip() {
        let kp = SealingKeypair::generate();
        let secret_data = b"the-data-encryption-key-32-bytes";
        let blob = seal_to(&kp.public_bytes(), secret_data);
        assert_eq!(kp.open(&blob).as_deref(), Some(secret_data.as_slice()));
    }

    #[test]
    fn wrong_recipient_cannot_open() {
        let alice = SealingKeypair::generate();
        let bob = SealingKeypair::generate();
        let blob = seal_to(&alice.public_bytes(), b"secret");
        assert!(bob.open(&blob).is_none());
    }

    #[test]
    fn tampered_ciphertext_fails_aead() {
        let kp = SealingKeypair::generate();
        let mut blob = seal_to(&kp.public_bytes(), b"secret");
        blob.ciphertext[0] ^= 0xff;
        assert!(kp.open(&blob).is_none());
    }

    #[test]
    fn at_rest_symmetric_roundtrip() {
        let key = [3u8; 32];
        let blob = encrypt_at_rest(&key, b"parquet-bytes");
        assert_eq!(
            decrypt_at_rest(&key, &blob).as_deref(),
            Some(b"parquet-bytes".as_slice())
        );
        // wrong key fails
        assert!(decrypt_at_rest(&[9u8; 32], &blob).is_none());
    }

    #[test]
    fn hex_roundtrip() {
        let kp = SealingKeypair::generate();
        let blob = seal_to(&kp.public_bytes(), b"secret");
        let s = blob.to_hex();
        let back = SealedBlob::from_hex(&s).unwrap();
        assert_eq!(blob, back);
        assert_eq!(kp.open(&back).as_deref(), Some(b"secret".as_slice()));
    }
}
