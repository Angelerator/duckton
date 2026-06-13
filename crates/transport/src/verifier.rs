//! Custom rustls certificate verifiers that pin connections to node identities
//! (architecture §6: "no CA, TOFU + allowlist").
//!
//! Both directions of the mutual-TLS handshake use the same policy:
//!  * **TOFU** — accept any validly-presented self-signed cert; the caller
//!    records the derived `node_id` for application use.
//!  * **Allowlist** — only accept certs whose derived `node_id` is allowlisted.
//!
//! Possession of the private key is proven by the standard TLS CertificateVerify
//! step (delegated to the ring provider's signature verification), so a derived
//! `node_id` is always authenticated — a peer cannot present a key it doesn't own.

use std::collections::HashSet;
use std::sync::Arc;

use p2p_proto::NodeId;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};

use p2p_config::PinningMode;

/// Extract the node id from a presented certificate by reading the raw Ed25519
/// public key out of its SubjectPublicKeyInfo and hashing it with BLAKE3.
pub fn node_id_from_cert(cert: &CertificateDer<'_>) -> Result<NodeId, String> {
    let (_, parsed) = x509_parser::parse_x509_certificate(cert.as_ref())
        .map_err(|e| format!("x509 parse: {e}"))?;
    let spki = parsed.public_key();
    let key_bytes = spki.subject_public_key.data.as_ref();
    // Ed25519 SPKI subjectPublicKey is the raw 32-byte key.
    if key_bytes.len() != 32 {
        return Err(format!(
            "expected 32-byte ed25519 key, got {} bytes",
            key_bytes.len()
        ));
    }
    Ok(NodeId::from_pubkey(key_bytes))
}

/// Shared pinning policy used by both verifier roles.
#[derive(Debug, Clone)]
pub struct PinPolicy {
    mode: PinningMode,
    allowlist: HashSet<String>,
}

impl PinPolicy {
    pub fn new(mode: PinningMode, allowlist: impl IntoIterator<Item = String>) -> Self {
        Self {
            mode,
            allowlist: allowlist.into_iter().collect(),
        }
    }

    fn check(&self, cert: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        let node_id = node_id_from_cert(cert)
            .map_err(|e| rustls::Error::General(format!("identity extraction failed: {e}")))?;
        match self.mode {
            PinningMode::Tofu => Ok(()),
            PinningMode::Allowlist => {
                if self.allowlist.contains(node_id.as_str()) {
                    Ok(())
                } else {
                    Err(rustls::Error::General(format!(
                        "peer {node_id} not in allowlist"
                    )))
                }
            }
        }
    }
}

/// Verifier used by the *client* role to validate the *server's* certificate.
#[derive(Debug)]
pub struct PinnedServerVerifier {
    policy: PinPolicy,
    algs: WebPkiSupportedAlgorithms,
}

impl PinnedServerVerifier {
    pub fn new(policy: PinPolicy) -> Arc<Self> {
        Arc::new(Self {
            policy,
            algs: rustls::crypto::ring::default_provider().signature_verification_algorithms,
        })
    }
}

impl ServerCertVerifier for PinnedServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.policy.check(end_entity)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// Verifier used by the *server* role to validate the *client's* certificate
/// (mutual TLS). Client auth is mandatory.
#[derive(Debug)]
pub struct PinnedClientVerifier {
    policy: PinPolicy,
    algs: WebPkiSupportedAlgorithms,
    empty_subjects: Vec<DistinguishedName>,
}

impl PinnedClientVerifier {
    pub fn new(policy: PinPolicy) -> Arc<Self> {
        Arc::new(Self {
            policy,
            algs: rustls::crypto::ring::default_provider().signature_verification_algorithms,
            empty_subjects: Vec::new(),
        })
    }
}

impl ClientCertVerifier for PinnedClientVerifier {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &self.empty_subjects
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.policy.check(end_entity)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::NodeIdentity;

    #[test]
    fn allowlist_rejects_unknown_and_accepts_known() {
        let id = NodeIdentity::generate().unwrap();
        let cert = id.cert_chain()[0].clone();

        let deny = PinPolicy::new(PinningMode::Allowlist, vec!["b3:other".to_string()]);
        assert!(deny.check(&cert).is_err());

        let allow = PinPolicy::new(PinningMode::Allowlist, vec![id.node_id().to_string()]);
        assert!(allow.check(&cert).is_ok());
    }

    #[test]
    fn tofu_accepts_any() {
        let id = NodeIdentity::generate().unwrap();
        let policy = PinPolicy::new(PinningMode::Tofu, Vec::<String>::new());
        assert!(policy.check(&id.cert_chain()[0]).is_ok());
    }
}
