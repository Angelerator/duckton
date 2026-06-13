//! Node identity: an Ed25519 keypair that is *both* the TLS certificate key and
//! the signing key for receipts/vouches (architecture §6).
//!
//! `node_id = "b3:" + hex(BLAKE3(ed25519_public_key))`.
//!
//! The same key is used to mint a self-signed X.509 certificate (via `rcgen`)
//! presented during the mTLS handshake. Because the cert's SubjectPublicKeyInfo
//! carries the raw Ed25519 public key, a peer can extract it from the presented
//! cert and derive the exact same `node_id` — binding the TLS identity to the
//! node identity with no certificate authority.

use ed25519_dalek::pkcs8::{DecodePrivateKey, EncodePrivateKey};
use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use p2p_proto::NodeId;
use rand::rngs::OsRng;
use rustls_pki_types::{CertificateDer, PrivatePkcs8KeyDer};

use crate::error::{Result, TransportError};

/// A node's cryptographic identity plus its self-signed TLS certificate.
#[derive(Clone)]
pub struct NodeIdentity {
    signing_key: SigningKey,
    node_id: NodeId,
    cert_der: Vec<u8>,
    key_pkcs8_der: Vec<u8>,
}

impl NodeIdentity {
    /// Generate a fresh random identity (ephemeral; used in tests and when no
    /// `identity.key_path` is configured).
    pub fn generate() -> Result<Self> {
        let signing_key = SigningKey::generate(&mut OsRng);
        Self::from_signing_key(signing_key)
    }

    /// Load an identity from a PKCS#8 Ed25519 PEM file.
    pub fn from_pem_file(path: &str) -> Result<Self> {
        let pem = std::fs::read_to_string(path)
            .map_err(|e| TransportError::Identity(format!("read {path}: {e}")))?;
        let signing_key = SigningKey::from_pkcs8_pem(&pem)
            .map_err(|e| TransportError::Identity(format!("parse key {path}: {e}")))?;
        Self::from_signing_key(signing_key)
    }

    /// Serialize the private key to PKCS#8 PEM (for persisting an identity).
    pub fn to_pem(&self) -> Result<String> {
        Ok(self
            .signing_key
            .to_pkcs8_pem(Default::default())
            .map_err(|e| TransportError::Identity(e.to_string()))?
            .to_string())
    }

    /// Build an identity (and its self-signed cert) from an Ed25519 signing key.
    pub fn from_signing_key(signing_key: SigningKey) -> Result<Self> {
        let pubkey = signing_key.verifying_key();
        let node_id = NodeId::from_pubkey(pubkey.as_bytes());

        // Export the Ed25519 key as PKCS#8 DER and hand it to rcgen so the TLS
        // cert is signed by (and bound to) the node identity key.
        let pkcs8 = signing_key
            .to_pkcs8_der()
            .map_err(|e| TransportError::Identity(format!("pkcs8 encode: {e}")))?;
        let key_pkcs8_der = pkcs8.as_bytes().to_vec();

        let pki_key = PrivatePkcs8KeyDer::from(key_pkcs8_der.as_slice());
        let rcgen_key =
            rcgen::KeyPair::from_pkcs8_der_and_sign_algo(&pki_key, &rcgen::PKCS_ED25519)
                .map_err(|e| TransportError::Identity(format!("rcgen key: {e}")))?;

        // Subject Alternative Name carries the node id (informational; identity
        // is authenticated cryptographically, not by SAN matching).
        let mut params = rcgen::CertificateParams::new(vec!["duckdb-p2p".to_string()])
            .map_err(|e| TransportError::Identity(format!("cert params: {e}")))?;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, node_id.as_str());

        let cert = params
            .self_signed(&rcgen_key)
            .map_err(|e| TransportError::Identity(format!("self_signed: {e}")))?;
        let cert_der = cert.der().to_vec();

        Ok(Self {
            signing_key,
            node_id,
            cert_der,
            key_pkcs8_der,
        })
    }

    pub fn node_id(&self) -> &NodeId {
        &self.node_id
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing_key.verifying_key()
    }

    /// Raw 32-byte Ed25519 public key.
    pub fn public_key_bytes(&self) -> [u8; 32] {
        self.signing_key.verifying_key().to_bytes()
    }

    /// Sign an arbitrary message with the node identity key (used for receipts,
    /// vouches, capability tokens in higher layers).
    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.signing_key.sign(msg).to_bytes()
    }

    /// Verify a signature against this identity's own public key.
    pub fn verify(&self, msg: &[u8], sig: &[u8; 64]) -> bool {
        let sig = ed25519_dalek::Signature::from_bytes(sig);
        self.signing_key
            .verifying_key()
            .verify(msg, &sig)
            .is_ok()
    }

    /// The certificate chain to present in TLS (single self-signed cert).
    pub fn cert_chain(&self) -> Vec<CertificateDer<'static>> {
        vec![CertificateDer::from(self.cert_der.clone())]
    }

    /// The private key in PKCS#8 DER form for the rustls config.
    pub fn private_key_der(&self) -> PrivatePkcs8KeyDer<'static> {
        PrivatePkcs8KeyDer::from(self.key_pkcs8_der.clone())
    }
}

impl std::fmt::Debug for NodeIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeIdentity")
            .field("node_id", &self.node_id)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_yields_consistent_node_id() {
        let id = NodeIdentity::generate().unwrap();
        let expected = NodeId::from_pubkey(&id.public_key_bytes());
        assert_eq!(id.node_id(), &expected);
        assert!(id.node_id().as_str().starts_with("b3:"));
        assert!(!id.cert_chain().is_empty());
    }

    #[test]
    fn sign_and_verify_roundtrip() {
        let id = NodeIdentity::generate().unwrap();
        let sig = id.sign(b"hello");
        assert!(id.verify(b"hello", &sig));
        assert!(!id.verify(b"hellp", &sig));
    }

    #[test]
    fn pem_roundtrip_preserves_identity() {
        let id = NodeIdentity::generate().unwrap();
        let pem = id.to_pem().unwrap();
        let key = SigningKey::from_pkcs8_pem(&pem).unwrap();
        let id2 = NodeIdentity::from_signing_key(key).unwrap();
        assert_eq!(id.node_id(), id2.node_id());
    }

    #[test]
    fn cert_embeds_extractable_pubkey_matching_node_id() {
        // The peer-side extraction (used by the verifier) must derive the same
        // node id from the presented certificate.
        let id = NodeIdentity::generate().unwrap();
        let cert = &id.cert_chain()[0];
        let extracted = crate::verifier::node_id_from_cert(cert).unwrap();
        assert_eq!(&extracted, id.node_id());
    }
}
