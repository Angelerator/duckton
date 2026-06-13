//! Capability tokens (architecture §6, §12): attenuable, offline-verifiable
//! authorization, biscuit/macaroon-style.
//!
//! A token is a **delegation chain** of layers:
//!  * Layer 0 is signed by a trusted **issuer**; it names the first holder and a
//!    set of caveats (restrictions).
//!  * Each subsequent layer is signed by the *previous* layer's holder, delegates
//!    to a new holder, and may only **add** caveats (attenuation never widens
//!    authority). Each layer's signature binds the previous layer's signature,
//!    so the chain cannot be reordered or spliced.
//!
//! Verification needs only the trusted issuer's public key — no central server.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

const DOMAIN: &[u8] = b"duckdb-p2p-capability-v1";

/// A single restriction on a token's authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Caveat {
    /// Token is invalid after this unix-seconds timestamp.
    ExpiresAt(u64),
    /// Allowed operation (e.g. "query").
    Operation(String),
    /// Resource access is restricted to this prefix (e.g. "s3://bkt/events/").
    ResourcePrefix(String),
    /// Maximum number of rows the operation may scan/return.
    MaxRows(u64),
}

/// Context describing the action being authorized, checked against caveats.
#[derive(Debug, Clone)]
pub struct AuthContext {
    pub now: u64,
    pub operation: String,
    pub resource: String,
    pub rows: u64,
}

impl Caveat {
    fn satisfied_by(&self, ctx: &AuthContext) -> bool {
        match self {
            Caveat::ExpiresAt(t) => ctx.now <= *t,
            Caveat::Operation(op) => &ctx.operation == op,
            Caveat::ResourcePrefix(p) => ctx.resource.starts_with(p),
            Caveat::MaxRows(m) => ctx.rows <= *m,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Layer {
    /// Who this layer delegates authority to (hex ed25519 pubkey).
    holder_pubkey: String,
    caveats: Vec<Caveat>,
    /// Who signed this layer (hex ed25519 pubkey): issuer for layer 0, else the
    /// previous layer's holder.
    signer_pubkey: String,
    /// Hex ed25519 signature over this layer's canonical bytes.
    sig: String,
}

/// An attenuable capability token (a chain of delegation layers).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityToken {
    layers: Vec<Layer>,
}

/// Errors from token verification.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TokenError {
    #[error("empty token")]
    Empty,
    #[error("issuer mismatch")]
    IssuerMismatch,
    #[error("broken delegation chain at layer {0}")]
    BrokenChain(usize),
    #[error("invalid signature at layer {0}")]
    BadSignature(usize),
    #[error("malformed key/signature encoding")]
    Encoding,
    #[error("caveat not satisfied")]
    CaveatFailed,
    #[error("holder key does not match current holder")]
    NotHolder,
}

fn layer_bytes(
    index: usize,
    holder_pubkey: &[u8; 32],
    caveats: &[Caveat],
    prev_sig: &[u8],
) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(DOMAIN);
    buf.extend_from_slice(&(index as u64).to_le_bytes());
    buf.extend_from_slice(holder_pubkey);
    // canonical caveat encoding via serde_json (stable for our enum)
    let cav = serde_json::to_vec(caveats).expect("caveats serialize");
    buf.extend_from_slice(&(cav.len() as u64).to_le_bytes());
    buf.extend_from_slice(&cav);
    buf.extend_from_slice(&(prev_sig.len() as u64).to_le_bytes());
    buf.extend_from_slice(prev_sig);
    buf
}

fn decode_key(hex_s: &str) -> Result<[u8; 32], TokenError> {
    let b = hex::decode(hex_s).map_err(|_| TokenError::Encoding)?;
    b.try_into().map_err(|_| TokenError::Encoding)
}

fn decode_sig(hex_s: &str) -> Result<[u8; 64], TokenError> {
    let b = hex::decode(hex_s).map_err(|_| TokenError::Encoding)?;
    b.try_into().map_err(|_| TokenError::Encoding)
}

impl CapabilityToken {
    /// Mint a root token signed by the issuer, delegating to `holder_pubkey`.
    pub fn mint(issuer: &SigningKey, holder_pubkey: &[u8; 32], caveats: Vec<Caveat>) -> Self {
        let issuer_pk = issuer.verifying_key().to_bytes();
        let msg = layer_bytes(0, holder_pubkey, &caveats, &[]);
        let sig = issuer.sign(&msg);
        CapabilityToken {
            layers: vec![Layer {
                holder_pubkey: hex::encode(holder_pubkey),
                caveats,
                signer_pubkey: hex::encode(issuer_pk),
                sig: hex::encode(sig.to_bytes()),
            }],
        }
    }

    /// The current holder pubkey (the last layer's delegate).
    pub fn holder(&self) -> Result<[u8; 32], TokenError> {
        let last = self.layers.last().ok_or(TokenError::Empty)?;
        decode_key(&last.holder_pubkey)
    }

    /// Attenuate the token: the current holder delegates to `delegate_pubkey`,
    /// adding `added_caveats` (which can only narrow authority).
    pub fn attenuate(
        &self,
        holder_key: &SigningKey,
        delegate_pubkey: &[u8; 32],
        added_caveats: Vec<Caveat>,
    ) -> Result<Self, TokenError> {
        let current_holder = self.holder()?;
        if holder_key.verifying_key().to_bytes() != current_holder {
            return Err(TokenError::NotHolder);
        }
        let prev_sig = decode_sig(&self.layers.last().unwrap().sig)?;
        let index = self.layers.len();
        let msg = layer_bytes(index, delegate_pubkey, &added_caveats, &prev_sig);
        let sig = holder_key.sign(&msg);
        let mut layers = self.layers.clone();
        layers.push(Layer {
            holder_pubkey: hex::encode(delegate_pubkey),
            caveats: added_caveats,
            signer_pubkey: hex::encode(current_holder),
            sig: hex::encode(sig.to_bytes()),
        });
        Ok(CapabilityToken { layers })
    }

    /// Verify the chain against the trusted issuer key and check every caveat
    /// against `ctx`. On success, returns the authorized holder pubkey.
    pub fn verify(
        &self,
        trusted_issuer_pubkey: &[u8; 32],
        ctx: &AuthContext,
    ) -> Result<[u8; 32], TokenError> {
        if self.layers.is_empty() {
            return Err(TokenError::Empty);
        }
        let mut prev_sig: Vec<u8> = Vec::new();
        let mut expected_signer = *trusted_issuer_pubkey;

        for (i, layer) in self.layers.iter().enumerate() {
            let signer = decode_key(&layer.signer_pubkey)?;
            if i == 0 {
                if signer != *trusted_issuer_pubkey {
                    return Err(TokenError::IssuerMismatch);
                }
            } else if signer != expected_signer {
                return Err(TokenError::BrokenChain(i));
            }

            let holder = decode_key(&layer.holder_pubkey)?;
            let sig = Signature::from_bytes(&decode_sig(&layer.sig)?);
            let vk = VerifyingKey::from_bytes(&signer).map_err(|_| TokenError::Encoding)?;
            let msg = layer_bytes(i, &holder, &layer.caveats, &prev_sig);
            vk.verify(&msg, &sig).map_err(|_| TokenError::BadSignature(i))?;

            // every caveat across all layers must hold
            for cav in &layer.caveats {
                if !cav.satisfied_by(ctx) {
                    return Err(TokenError::CaveatFailed);
                }
            }

            expected_signer = holder; // next layer must be signed by this holder
            prev_sig = decode_sig(&layer.sig)?.to_vec();
        }

        self.holder()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;

    fn ctx() -> AuthContext {
        AuthContext {
            now: 1000,
            operation: "query".into(),
            resource: "s3://bkt/events/2024.parquet".into(),
            rows: 100,
        }
    }

    #[test]
    fn mint_and_verify() {
        let issuer = SigningKey::generate(&mut OsRng);
        let holder = SigningKey::generate(&mut OsRng);
        let token = CapabilityToken::mint(
            &issuer,
            &holder.verifying_key().to_bytes(),
            vec![
                Caveat::Operation("query".into()),
                Caveat::ResourcePrefix("s3://bkt/events/".into()),
                Caveat::ExpiresAt(2000),
            ],
        );
        let who = token.verify(&issuer.verifying_key().to_bytes(), &ctx()).unwrap();
        assert_eq!(who, holder.verifying_key().to_bytes());
    }

    #[test]
    fn wrong_issuer_rejected() {
        let issuer = SigningKey::generate(&mut OsRng);
        let other = SigningKey::generate(&mut OsRng);
        let holder = SigningKey::generate(&mut OsRng);
        let token = CapabilityToken::mint(&issuer, &holder.verifying_key().to_bytes(), vec![]);
        assert_eq!(
            token.verify(&other.verifying_key().to_bytes(), &ctx()),
            Err(TokenError::IssuerMismatch)
        );
    }

    #[test]
    fn expired_token_fails() {
        let issuer = SigningKey::generate(&mut OsRng);
        let holder = SigningKey::generate(&mut OsRng);
        let token = CapabilityToken::mint(
            &issuer,
            &holder.verifying_key().to_bytes(),
            vec![Caveat::ExpiresAt(500)],
        );
        assert_eq!(
            token.verify(&issuer.verifying_key().to_bytes(), &ctx()),
            Err(TokenError::CaveatFailed)
        );
    }

    #[test]
    fn resource_outside_prefix_fails() {
        let issuer = SigningKey::generate(&mut OsRng);
        let holder = SigningKey::generate(&mut OsRng);
        let token = CapabilityToken::mint(
            &issuer,
            &holder.verifying_key().to_bytes(),
            vec![Caveat::ResourcePrefix("s3://other/".into())],
        );
        assert_eq!(
            token.verify(&issuer.verifying_key().to_bytes(), &ctx()),
            Err(TokenError::CaveatFailed)
        );
    }

    #[test]
    fn attenuation_narrows_and_verifies() {
        let issuer = SigningKey::generate(&mut OsRng);
        let holder = SigningKey::generate(&mut OsRng);
        let delegate = SigningKey::generate(&mut OsRng);
        let root = CapabilityToken::mint(
            &issuer,
            &holder.verifying_key().to_bytes(),
            vec![Caveat::Operation("query".into())],
        );
        // holder delegates to `delegate`, adding a row cap
        let attenuated = root
            .attenuate(&holder, &delegate.verifying_key().to_bytes(), vec![Caveat::MaxRows(1000)])
            .unwrap();
        let who = attenuated
            .verify(&issuer.verifying_key().to_bytes(), &ctx())
            .unwrap();
        assert_eq!(who, delegate.verifying_key().to_bytes());

        // the added caveat is enforced
        let mut over = ctx();
        over.rows = 5000;
        assert_eq!(
            attenuated.verify(&issuer.verifying_key().to_bytes(), &over),
            Err(TokenError::CaveatFailed)
        );
    }

    #[test]
    fn non_holder_cannot_attenuate() {
        let issuer = SigningKey::generate(&mut OsRng);
        let holder = SigningKey::generate(&mut OsRng);
        let impostor = SigningKey::generate(&mut OsRng);
        let root = CapabilityToken::mint(&issuer, &holder.verifying_key().to_bytes(), vec![]);
        let r = root.attenuate(&impostor, &impostor.verifying_key().to_bytes(), vec![]);
        assert_eq!(r, Err(TokenError::NotHolder));
    }

    #[test]
    fn tampered_caveat_breaks_signature() {
        let issuer = SigningKey::generate(&mut OsRng);
        let holder = SigningKey::generate(&mut OsRng);
        let mut token = CapabilityToken::mint(
            &issuer,
            &holder.verifying_key().to_bytes(),
            vec![Caveat::MaxRows(10)],
        );
        // tamper: widen the row cap after signing
        token.layers[0].caveats = vec![Caveat::MaxRows(1_000_000)];
        assert_eq!(
            token.verify(&issuer.verifying_key().to_bytes(), &ctx()),
            Err(TokenError::BadSignature(0))
        );
    }
}
