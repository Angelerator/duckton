//! `p2p-trust` — the trust engine (architecture §7).
//!
//! Independent, composable signals a requester combines into a trust decision:
//!  * **Verification** — [`canonical`]: deterministic result hashing + quorum.
//!  * **Reputation** — [`receipt`] + [`reputation`]: signed receipts feeding a
//!    pluggable, recency-weighted, bounded reputation store.
//!  * **Auditing** — [`canary`]: known-answer canary checks.
//!  * **Identity / Sybil resistance** — [`sybil`]: PoW identity minting + vouching.
//!  * **Authorization** — [`token`]: attenuable capability tokens.
//!  * **Attestation** — [`attestation`]: tiered attestor interface + mock + key sealing.
//!
//! This crate is transport-agnostic (no QUIC dependency); signing is abstracted
//! by the [`receipt::Signer`] trait.

pub mod antiabuse;
pub mod attestation;
pub mod canary;
pub mod canonical;
pub mod capability;
pub mod failure_detector;
pub mod persistent;
pub mod receipt;
pub mod reputation;
pub mod sealing;
pub mod sybil;
pub mod system;
pub mod token;

pub use antiabuse::{
    classify_failure, is_job_consensus_failure, is_nondeterministic, requester_trust_weight,
    sign_abuse_signal, verify_abuse_signal,
};
pub use attestation::{
    attestation_bound_pub, AllowlistVerifier, AttestError, AttestationVerifier, Attestor,
    MockAttestor,
};
pub use canonical::{
    canonical_hash, evaluate_quorum, evaluate_quorum_on_commits, CommitKey,
    FingerprintQuorumOutcome, QuorumOutcome,
};
pub use capability::{
    sign_capability_ad, sign_capability_profile, verify_capability_ad, verify_capability_profile,
    CapabilityDraft, CapabilityProfileDraft,
};
pub use failure_detector::PhiDetector;
pub use persistent::{RedbTrustStore, TrustStoreError};
pub use receipt::{sign_receipt, signing_bytes, verify_receipt, ReceiptDraft, Signer};
pub use reputation::{
    age_factor, attestation_gate, confidence_reputation, exploration_bonus, now_ts,
    soft_trust_score, InMemoryTrustStore, PerfAggregate, ProvenCapability, TrustInputs, TrustStore,
};
pub use sealing::{decrypt_at_rest, encrypt_at_rest, seal_to, SealedBlob, SealingKeypair};
pub use sybil::{make_vouch, mint_pow, verify_pow, verify_vouch, PowStamp, Vouch};
pub use system::{sign_system_profile, verify_system_profile};
pub use token::{
    verify_group_membership, verify_region_attestation, AuthContext, CapabilityToken, Caveat,
    TokenError,
};
