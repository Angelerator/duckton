//! Attestation tiers + verification (architecture §7.2, §9.3).
//!
//! Tiers: L0 (anonymous), L1 (TPM measured boot), L2 (hardware TEE). A worker
//! produces an [`Attestation`] (proto type) carrying evidence; a requester
//! verifies it against an allowlist of accepted measurements before trusting it
//! with sensitive data or releasing a sealed key.
//!
//! ## What is real vs. mocked
//! The [`Attestor`] / [`AttestationVerifier`] interfaces and the
//! evidence/nonce/measurement/bound-key shape mirror real remote attestation
//! (RATS/EAT): the verifier checks that an *unmodified, allowlisted* image runs,
//! and that a key is *bound* to that image. [`MockAttestor`] stands in for real
//! hardware: it signs evidence with a software "authority" key instead of a
//! TDX/SEV-SNP/Nitro quote signed by a vendor key.
//!
//! **Requires real hardware (not exercisable here):** producing a genuine TEE
//! quote and verifying it against vendor certificate chains. That plugs in
//! behind these same traits as a `TdxAttestor` / `NitroVerifier` etc.

use std::collections::HashSet;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use p2p_proto::{Attestation, AttestationLevel};
use serde::{Deserialize, Serialize};

const DOMAIN: &[u8] = b"duckdb-p2p-attestation-v1";

/// Errors from attestation verification.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AttestError {
    #[error("attestation level {got:?} below required {required:?}")]
    LevelTooLow {
        got: AttestationLevel,
        required: AttestationLevel,
    },
    #[error("measurement not in allowlist")]
    MeasurementNotAllowed,
    #[error("nonce mismatch (possible replay)")]
    NonceMismatch,
    #[error("bound key mismatch")]
    BoundKeyMismatch,
    #[error("untrusted attestation authority")]
    UntrustedAuthority,
    #[error("malformed or unverifiable evidence")]
    BadEvidence,
}

/// The decoded evidence carried in `Attestation.evidence` for the mock attestor.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct MockEvidence {
    measurement: String,
    nonce_hex: String,
    /// The key bound into the attestation (e.g. the enclave's X25519 sealing key).
    bound_pub_hex: String,
    authority_pub_hex: String,
    sig_hex: String,
}

fn evidence_signing_bytes(measurement: &str, nonce: &[u8], bound_pub: &[u8; 32]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(DOMAIN);
    buf.extend_from_slice(&(measurement.len() as u64).to_le_bytes());
    buf.extend_from_slice(measurement.as_bytes());
    buf.extend_from_slice(&(nonce.len() as u64).to_le_bytes());
    buf.extend_from_slice(nonce);
    buf.extend_from_slice(bound_pub);
    buf
}

/// Produces attestation evidence for this node's execution environment.
pub trait Attestor: Send + Sync {
    fn level(&self) -> AttestationLevel;
    /// Produce an attestation binding `bound_pub` (e.g. a sealing key) to the
    /// environment, freshened by `nonce`.
    fn produce(&self, nonce: &[u8], bound_pub: &[u8; 32]) -> Attestation;
}

/// Verifies attestation evidence against a policy.
pub trait AttestationVerifier: Send + Sync {
    fn verify(
        &self,
        att: &Attestation,
        nonce: &[u8],
        bound_pub: &[u8; 32],
    ) -> Result<(), AttestError>;
}

/// A software mock of a hardware attestor (stands in for TDX/SEV-SNP/Nitro).
pub struct MockAttestor {
    authority: SigningKey,
    measurement: String,
    level: AttestationLevel,
}

impl MockAttestor {
    pub fn new(authority: SigningKey, measurement: impl Into<String>, level: AttestationLevel) -> Self {
        Self {
            authority,
            measurement: measurement.into(),
            level,
        }
    }

    pub fn authority_pubkey(&self) -> [u8; 32] {
        self.authority.verifying_key().to_bytes()
    }

    pub fn measurement(&self) -> &str {
        &self.measurement
    }
}

impl Attestor for MockAttestor {
    fn level(&self) -> AttestationLevel {
        self.level
    }

    fn produce(&self, nonce: &[u8], bound_pub: &[u8; 32]) -> Attestation {
        let sig = self
            .authority
            .sign(&evidence_signing_bytes(&self.measurement, nonce, bound_pub));
        let evidence = MockEvidence {
            measurement: self.measurement.clone(),
            nonce_hex: hex::encode(nonce),
            bound_pub_hex: hex::encode(bound_pub),
            authority_pub_hex: hex::encode(self.authority_pubkey()),
            sig_hex: hex::encode(sig.to_bytes()),
        };
        Attestation {
            level: self.level,
            evidence: serde_json::to_vec(&evidence).expect("evidence serializes"),
            measurement: Some(self.measurement.clone()),
        }
    }
}

/// Verifier that accepts attestations signed by a trusted authority key whose
/// measurement is allowlisted and which meet a minimum level.
pub struct AllowlistVerifier {
    trusted_authority: [u8; 32],
    allowed_measurements: HashSet<String>,
    required_level: AttestationLevel,
}

impl AllowlistVerifier {
    pub fn new(
        trusted_authority: [u8; 32],
        allowed_measurements: impl IntoIterator<Item = String>,
        required_level: AttestationLevel,
    ) -> Self {
        Self {
            trusted_authority,
            allowed_measurements: allowed_measurements.into_iter().collect(),
            required_level,
        }
    }
}

impl AttestationVerifier for AllowlistVerifier {
    fn verify(
        &self,
        att: &Attestation,
        nonce: &[u8],
        bound_pub: &[u8; 32],
    ) -> Result<(), AttestError> {
        if att.level < self.required_level {
            return Err(AttestError::LevelTooLow {
                got: att.level,
                required: self.required_level,
            });
        }
        let evidence: MockEvidence =
            serde_json::from_slice(&att.evidence).map_err(|_| AttestError::BadEvidence)?;

        if !self.allowed_measurements.contains(&evidence.measurement) {
            return Err(AttestError::MeasurementNotAllowed);
        }
        if evidence.nonce_hex != hex::encode(nonce) {
            return Err(AttestError::NonceMismatch);
        }
        if evidence.bound_pub_hex != hex::encode(bound_pub) {
            return Err(AttestError::BoundKeyMismatch);
        }

        let authority_pub = decode32(&evidence.authority_pub_hex).ok_or(AttestError::BadEvidence)?;
        if authority_pub != self.trusted_authority {
            return Err(AttestError::UntrustedAuthority);
        }
        let vk = VerifyingKey::from_bytes(&authority_pub).map_err(|_| AttestError::BadEvidence)?;
        let sig_bytes = hex::decode(&evidence.sig_hex).map_err(|_| AttestError::BadEvidence)?;
        let sig_arr: [u8; 64] = sig_bytes.try_into().map_err(|_| AttestError::BadEvidence)?;
        vk.verify(
            &evidence_signing_bytes(&evidence.measurement, nonce, bound_pub),
            &Signature::from_bytes(&sig_arr),
        )
        .map_err(|_| AttestError::BadEvidence)?;
        Ok(())
    }
}

fn decode32(hex_s: &str) -> Option<[u8; 32]> {
    hex::decode(hex_s).ok()?.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;

    fn setup() -> (MockAttestor, [u8; 32]) {
        let authority = SigningKey::generate(&mut OsRng);
        let pub_ = authority.verifying_key().to_bytes();
        (
            MockAttestor::new(authority, "duckdb-enclave-v1", AttestationLevel::L2),
            pub_,
        )
    }

    #[test]
    fn valid_attestation_verifies() {
        let (attestor, authority) = setup();
        let nonce = [9u8; 16];
        let bound = [5u8; 32];
        let att = attestor.produce(&nonce, &bound);
        let v = AllowlistVerifier::new(
            authority,
            ["duckdb-enclave-v1".to_string()],
            AttestationLevel::L2,
        );
        assert_eq!(v.verify(&att, &nonce, &bound), Ok(()));
    }

    #[test]
    fn unlisted_measurement_rejected() {
        let (attestor, authority) = setup();
        let att = attestor.produce(&[1u8; 16], &[2u8; 32]);
        let v = AllowlistVerifier::new(authority, ["other-image".to_string()], AttestationLevel::L2);
        assert_eq!(
            v.verify(&att, &[1u8; 16], &[2u8; 32]),
            Err(AttestError::MeasurementNotAllowed)
        );
    }

    #[test]
    fn replayed_nonce_rejected() {
        let (attestor, authority) = setup();
        let att = attestor.produce(&[1u8; 16], &[2u8; 32]);
        let v = AllowlistVerifier::new(
            authority,
            ["duckdb-enclave-v1".to_string()],
            AttestationLevel::L2,
        );
        // verifier expects a different (fresh) nonce
        assert_eq!(
            v.verify(&att, &[7u8; 16], &[2u8; 32]),
            Err(AttestError::NonceMismatch)
        );
    }

    #[test]
    fn untrusted_authority_rejected() {
        let (attestor, _authority) = setup();
        let att = attestor.produce(&[1u8; 16], &[2u8; 32]);
        let other = SigningKey::generate(&mut OsRng).verifying_key().to_bytes();
        let v = AllowlistVerifier::new(other, ["duckdb-enclave-v1".to_string()], AttestationLevel::L2);
        assert_eq!(
            v.verify(&att, &[1u8; 16], &[2u8; 32]),
            Err(AttestError::UntrustedAuthority)
        );
    }

    #[test]
    fn level_below_required_rejected() {
        let authority = SigningKey::generate(&mut OsRng);
        let authority_pub = authority.verifying_key().to_bytes();
        // attestor only reaches L1
        let attestor = MockAttestor::new(authority, "img", AttestationLevel::L1);
        let att = attestor.produce(&[1u8; 16], &[2u8; 32]);
        let v = AllowlistVerifier::new(authority_pub, ["img".to_string()], AttestationLevel::L2);
        assert!(matches!(
            v.verify(&att, &[1u8; 16], &[2u8; 32]),
            Err(AttestError::LevelTooLow { .. })
        ));
    }
}
