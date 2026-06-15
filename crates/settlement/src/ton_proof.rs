//! `ton_proof` (ton-proof-item-v2) two-way wallet <-> node binding verification,
//! in pure Rust (Ed25519 + SHA-256). BLOCKCHAIN_ECONOMICS §3.1.
//!
//! Direction 2 (wallet attests node) is the standard TON Connect proof:
//! ```text
//! message   = "ton-proof-item-v2/"
//!           ‖ workchain (int32, big-endian, 4 bytes)
//!           ‖ account_id (32 bytes)
//!           ‖ domain_len (uint32, little-endian, 4 bytes)
//!           ‖ domain (utf8)
//!           ‖ timestamp (uint64, little-endian, 8 bytes)
//!           ‖ payload (bytes)
//! signature = Ed25519( sha256( 0xffff ‖ "ton-connect" ‖ sha256(message) ) )
//! ```
//! Direction 1 (node attests wallet) reuses the node identity key over a
//! domain-separated bind message.

use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256};

use crate::types::{BindingError, NodeWalletBinding, TonProof, WalletAddress};
use crate::wallet::WalletV5R1;

const TON_PROOF_PREFIX: &[u8] = b"ton-proof-item-v2/";
const TON_CONNECT_PREFIX: &[u8] = b"ton-connect";
const NODE_BIND_DOMAIN: &[u8] = b"duckdb-p2p-wallet-bind-v1";

/// The app domain this grid expects inside a `ton_proof` (TON Connect domain
/// binding). A proof a user signed for any *other* dApp will not match, so a
/// cross-app proof cannot be replayed here.
pub const EXPECTED_TON_PROOF_DOMAIN: &str = "duckdb-p2p";

/// Maximum accepted age of a `ton_proof` timestamp — the TON Connect standard
/// backend window (15 minutes). Older proofs are rejected as replays.
pub const MAX_TON_PROOF_AGE_SECS: u64 = 15 * 60;

/// Tolerance for a `ton_proof` timestamp in the future (clock skew).
pub const TON_PROOF_FUTURE_SKEW_SECS: u64 = 5 * 60;

/// Whether `wallet_pubkey` actually owns `addr`, by re-deriving the standard
/// wallet **v5r1** address (testnet or mainnet) from the key and comparing. This
/// binds the key to the address: an attacker cannot present their own keypair
/// alongside a victim's address. (The grid standardizes on wallet v5r1; other
/// wallet versions are intentionally not accepted for binding.)
fn wallet_pubkey_owns_address(wallet_pubkey: &[u8; 32], addr: &WalletAddress) -> bool {
    WalletV5R1::testnet(*wallet_pubkey).address() == *addr
        || WalletV5R1::mainnet(*wallet_pubkey).address() == *addr
}

/// Assemble the `ton-proof-item-v2` message that the wallet key signs.
pub fn ton_proof_message(addr: &WalletAddress, proof: &TonProof) -> Vec<u8> {
    let mut m = Vec::new();
    m.extend_from_slice(TON_PROOF_PREFIX);
    m.extend_from_slice(&addr.workchain.to_be_bytes());
    m.extend_from_slice(&addr.hash);
    m.extend_from_slice(&(proof.domain.len() as u32).to_le_bytes());
    m.extend_from_slice(proof.domain.as_bytes());
    m.extend_from_slice(&proof.timestamp.to_le_bytes());
    m.extend_from_slice(&proof.payload);
    m
}

/// The final 32-byte digest the Ed25519 signature is verified against.
pub fn ton_proof_signing_hash(addr: &WalletAddress, proof: &TonProof) -> [u8; 32] {
    let inner = Sha256::digest(ton_proof_message(addr, proof));
    let mut h = Sha256::new();
    h.update([0xff, 0xff]);
    h.update(TON_CONNECT_PREFIX);
    h.update(inner);
    let out = h.finalize();
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&out);
    digest
}

/// Verify a stand-alone `ton_proof` against a wallet public key, enforcing the
/// full TON Connect verification policy (not just the signature):
///  * the proof's `domain` matches `expected_domain` (no cross-app reuse),
///  * the proof's `timestamp` is fresh (within [`MAX_TON_PROOF_AGE_SECS`] of
///    `now`, and not implausibly in the future),
///  * `wallet_pubkey` actually owns `addr` (re-derived v5r1 address), and
///  * the Ed25519 signature is valid (`verify_strict`).
pub fn verify_ton_proof(
    addr: &WalletAddress,
    wallet_pubkey: &[u8; 32],
    proof: &TonProof,
    expected_domain: &str,
    now: u64,
) -> Result<(), BindingError> {
    // Domain binding: reject a proof signed for a different dApp.
    if proof.domain != expected_domain {
        return Err(BindingError::DomainMismatch);
    }
    // Freshness: reject stale (replay) or implausibly-future timestamps.
    if now.saturating_sub(proof.timestamp) > MAX_TON_PROOF_AGE_SECS
        || proof.timestamp > now.saturating_add(TON_PROOF_FUTURE_SKEW_SECS)
    {
        return Err(BindingError::ProofExpired);
    }
    // Key↔address binding: the pubkey must actually own the claimed address.
    if !wallet_pubkey_owns_address(wallet_pubkey, addr) {
        return Err(BindingError::WalletAddressMismatch);
    }
    let vk = VerifyingKey::from_bytes(wallet_pubkey).map_err(|_| BindingError::BadKeyOrSig)?;
    let sig_bytes: [u8; 64] = proof
        .signature
        .as_slice()
        .try_into()
        .map_err(|_| BindingError::BadKeyOrSig)?;
    let sig = Signature::from_bytes(&sig_bytes);
    let digest = ton_proof_signing_hash(addr, proof);
    vk.verify_strict(&digest, &sig)
        .map_err(|_| BindingError::TonProofInvalid)
}

/// The domain-separated message the NODE identity key signs in direction 1.
pub fn node_bind_message(addr: &WalletAddress, nonce: &[u8], expiry: u64) -> Vec<u8> {
    let mut m = Vec::new();
    m.extend_from_slice(NODE_BIND_DOMAIN);
    m.extend_from_slice(&addr.to_raw_bytes());
    m.extend_from_slice(nonce);
    m.extend_from_slice(&expiry.to_le_bytes());
    m
}

/// Verify the full two-way [`NodeWalletBinding`] (both directions), at `now`.
pub fn verify_binding(b: &NodeWalletBinding, now: u64) -> Result<(), BindingError> {
    // Time-box (direction 1 carries the expiry).
    if b.expiry != 0 && now > b.expiry {
        return Err(BindingError::Expired);
    }

    // node_id must be derived from the node public key (b3:BLAKE3(pubkey)).
    let expected_node_id = format!(
        "b3:{}",
        hex::encode(blake3::hash(&b.node_pubkey).as_bytes())
    );
    if expected_node_id != b.node_id {
        return Err(BindingError::NodeIdMismatch);
    }

    // Direction 1: node attests the wallet.
    let node_vk =
        VerifyingKey::from_bytes(&b.node_pubkey).map_err(|_| BindingError::BadKeyOrSig)?;
    let node_sig_bytes: [u8; 64] = b
        .sig_node
        .as_slice()
        .try_into()
        .map_err(|_| BindingError::BadKeyOrSig)?;
    let node_sig = Signature::from_bytes(&node_sig_bytes);
    let msg1 = node_bind_message(&b.wallet_address, &b.nonce, b.expiry);
    node_vk
        .verify_strict(&msg1, &node_sig)
        .map_err(|_| BindingError::NodeSigInvalid)?;

    // The ton_proof payload must bind THIS node and nonce (node_id ‖ nonce).
    let mut expected_payload = b.node_id.as_bytes().to_vec();
    expected_payload.extend_from_slice(&b.nonce);
    if b.ton_proof.payload != expected_payload {
        return Err(BindingError::PayloadMismatch);
    }

    // Direction 2: wallet attests the node via ton_proof — full TON Connect
    // policy (domain + freshness + key↔address + signature).
    verify_ton_proof(
        &b.wallet_address,
        &b.wallet_pubkey,
        &b.ton_proof,
        EXPECTED_TON_PROOF_DOMAIN,
        now,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    const NOW: u64 = 1_700_000_000;

    /// The wallet address that `pk` actually owns (v5r1 testnet) — so the
    /// key↔address binding check passes.
    fn owned_addr(pk: &[u8; 32]) -> WalletAddress {
        WalletV5R1::testnet(*pk).address()
    }

    fn signed_proof(sk: &SigningKey, addr: &WalletAddress, payload: Vec<u8>, ts: u64) -> TonProof {
        let mut proof = TonProof {
            domain: EXPECTED_TON_PROOF_DOMAIN.into(),
            timestamp: ts,
            payload,
            signature: vec![],
        };
        let digest = ton_proof_signing_hash(addr, &proof);
        proof.signature = sk.sign(&digest).to_bytes().to_vec();
        proof
    }

    #[test]
    fn ton_proof_roundtrip_verifies() {
        let sk = SigningKey::from_bytes(&[3u8; 32]);
        let pk = sk.verifying_key().to_bytes();
        let addr = owned_addr(&pk);
        let proof = signed_proof(&sk, &addr, b"hello".to_vec(), NOW);
        assert!(verify_ton_proof(&addr, &pk, &proof, EXPECTED_TON_PROOF_DOMAIN, NOW).is_ok());

        // Tampering with the timestamp breaks the proof signature.
        let mut bad = proof.clone();
        bad.timestamp += 1;
        assert_eq!(
            verify_ton_proof(&addr, &pk, &bad, EXPECTED_TON_PROOF_DOMAIN, NOW),
            Err(BindingError::TonProofInvalid)
        );
    }

    #[test]
    fn rejects_wrong_domain() {
        let sk = SigningKey::from_bytes(&[3u8; 32]);
        let pk = sk.verifying_key().to_bytes();
        let addr = owned_addr(&pk);
        let proof = signed_proof(&sk, &addr, b"hello".to_vec(), NOW);
        // A proof signed for this grid, checked against a different expected domain.
        assert_eq!(
            verify_ton_proof(&addr, &pk, &proof, "evil.example.com", NOW),
            Err(BindingError::DomainMismatch)
        );
    }

    #[test]
    fn rejects_stale_and_future_proof() {
        let sk = SigningKey::from_bytes(&[3u8; 32]);
        let pk = sk.verifying_key().to_bytes();
        let addr = owned_addr(&pk);
        // Stale: older than the 15-minute window.
        let stale = signed_proof(&sk, &addr, b"x".to_vec(), NOW - MAX_TON_PROOF_AGE_SECS - 1);
        assert_eq!(
            verify_ton_proof(&addr, &pk, &stale, EXPECTED_TON_PROOF_DOMAIN, NOW),
            Err(BindingError::ProofExpired)
        );
        // Implausibly in the future.
        let future = signed_proof(
            &sk,
            &addr,
            b"x".to_vec(),
            NOW + TON_PROOF_FUTURE_SKEW_SECS + 60,
        );
        assert_eq!(
            verify_ton_proof(&addr, &pk, &future, EXPECTED_TON_PROOF_DOMAIN, NOW),
            Err(BindingError::ProofExpired)
        );
    }

    #[test]
    fn rejects_pubkey_not_owning_address() {
        // Attacker presents their own key but a victim's (unrelated) address.
        let attacker = SigningKey::from_bytes(&[7u8; 32]);
        let pk = attacker.verifying_key().to_bytes();
        let victim_addr = WalletAddress::new(0, [9u8; 32]); // not derived from `pk`
        let proof = signed_proof(&attacker, &victim_addr, b"x".to_vec(), NOW);
        assert_eq!(
            verify_ton_proof(&victim_addr, &pk, &proof, EXPECTED_TON_PROOF_DOMAIN, NOW),
            Err(BindingError::WalletAddressMismatch)
        );
    }

    fn make_binding(
        node_sk: &SigningKey,
        wallet_sk: &SigningKey,
        now_expiry: u64,
    ) -> NodeWalletBinding {
        let node_pk = node_sk.verifying_key().to_bytes();
        let wallet_pk = wallet_sk.verifying_key().to_bytes();
        let node_id = format!("b3:{}", hex::encode(blake3::hash(&node_pk).as_bytes()));
        let addr = owned_addr(&wallet_pk);
        let nonce = b"nonce-123".to_vec();

        // Direction 1.
        let msg1 = node_bind_message(&addr, &nonce, now_expiry);
        let sig_node = node_sk.sign(&msg1).to_bytes().to_vec();

        // Direction 2: payload = node_id ‖ nonce.
        let mut payload = node_id.as_bytes().to_vec();
        payload.extend_from_slice(&nonce);
        let mut proof = TonProof {
            domain: "duckdb-p2p".into(),
            timestamp: 1_700_000_000,
            payload,
            signature: vec![],
        };
        let digest = ton_proof_signing_hash(&addr, &proof);
        proof.signature = wallet_sk.sign(&digest).to_bytes().to_vec();

        NodeWalletBinding {
            node_id,
            wallet_address: addr,
            node_pubkey: node_pk,
            wallet_pubkey: wallet_pk,
            nonce,
            expiry: now_expiry,
            sig_node,
            ton_proof: proof,
        }
    }

    #[test]
    fn two_way_binding_verifies() {
        let node_sk = SigningKey::from_bytes(&[1u8; 32]);
        let wallet_sk = SigningKey::from_bytes(&[2u8; 32]);
        let b = make_binding(&node_sk, &wallet_sk, 2_000_000_000);
        assert_eq!(verify_binding(&b, 1_700_000_000), Ok(()));
    }

    #[test]
    fn rejects_wrong_node_id() {
        let node_sk = SigningKey::from_bytes(&[1u8; 32]);
        let wallet_sk = SigningKey::from_bytes(&[2u8; 32]);
        let mut b = make_binding(&node_sk, &wallet_sk, 2_000_000_000);
        b.node_id = "b3:deadbeef".into();
        assert_eq!(
            verify_binding(&b, 1_700_000_000),
            Err(BindingError::NodeIdMismatch)
        );
    }

    #[test]
    fn rejects_expired() {
        let node_sk = SigningKey::from_bytes(&[1u8; 32]);
        let wallet_sk = SigningKey::from_bytes(&[2u8; 32]);
        let b = make_binding(&node_sk, &wallet_sk, 1000);
        assert_eq!(verify_binding(&b, 2000), Err(BindingError::Expired));
    }

    #[test]
    fn rejects_swapped_signatures() {
        // A wallet signature that does not match the wallet key fails direction 2.
        let node_sk = SigningKey::from_bytes(&[1u8; 32]);
        let wallet_sk = SigningKey::from_bytes(&[2u8; 32]);
        let other = SigningKey::from_bytes(&[5u8; 32]);
        let mut b = make_binding(&node_sk, &wallet_sk, 2_000_000_000);
        // Re-sign the proof with the wrong key.
        let digest = ton_proof_signing_hash(&b.wallet_address, &b.ton_proof);
        b.ton_proof.signature = other.sign(&digest).to_bytes().to_vec();
        assert_eq!(
            verify_binding(&b, 1_700_000_000),
            Err(BindingError::TonProofInvalid)
        );
    }
}
