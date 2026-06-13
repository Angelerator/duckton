//! Pluggable settlement seams (BLOCKCHAIN_ECONOMICS §10.1). Mirrors the existing
//! trait-per-collaborator style: every trait has a deterministic `mock` impl
//! (tests), a genuine `noop` impl (the free, no-chain path), and a `ton` impl
//! that talks to the on-chain contracts.

use p2p_config::DataClassCfg;
use p2p_proto::{JobId, NodeId};

use crate::types::{
    Amount, EscrowHandle, Hash32, InclusionProof, JobRecord, NodeWalletBinding, SettleError,
    SettlementOutcome, SlashError, SlashReason, WalletAddress,
};

/// Bind & verify wallet <-> node identity (§3). Verification is offline (pure
/// Ed25519/sha256), consistent with the project's no-central-auth principle.
pub trait Wallet: Send + Sync {
    /// This node's bound wallet address.
    fn address(&self) -> WalletAddress;
    /// Verify a two-way binding (both directions), at clock `now` (unix secs).
    fn verify_binding(&self, b: &NodeWalletBinding, now: u64) -> bool;
}

/// The money rail (§10.1). PAID jobs only — free jobs use [`crate::NoopSettlement`].
pub trait Settlement: Send + Sync {
    /// Lock the requester's max bid `B` in a per-job non-custodial escrow.
    fn open_escrow(&self, job: &JobId, max_bid: Amount) -> Result<EscrowHandle, SettleError>;
    /// Release escrow per the quorum verdict (HTLC-style, keyed on result hash).
    fn settle(&self, h: &EscrowHandle, outcome: &SettlementOutcome) -> Result<(), SettleError>;
    /// Refund the full escrow to the requester (e.g. on timeout / no quorum).
    fn refund(&self, h: &EscrowHandle) -> Result<(), SettleError>;
    /// True if this rail performs real on-chain/value movement. The free path's
    /// no-op returns `false` so callers can assert "no chain for free jobs".
    fn is_onchain(&self) -> bool;
}

/// Stake / bond / slash (§8). Feeds the EXISTING `TrustInputs.stake_factor`.
pub trait StakeRegistry: Send + Sync {
    fn stake_of(&self, node: &NodeId) -> Amount;
    fn is_eligible(&self, node: &NodeId, class: DataClassCfg) -> bool;
    /// Diminishing, capped ranking factor in `[0,1]` (§5.2).
    fn stake_factor(&self, node: &NodeId) -> f64;
    fn slash(&self, node: &NodeId, reason: SlashReason, amount: Amount) -> Result<(), SlashError>;
    fn request_unbond(&self, node: &NodeId, amount: Amount) -> Result<(), SlashError>;
}

/// Tamper-proof record anchoring: off-chain tree, on-chain root (§7).
pub trait RecordAnchor: Send + Sync {
    fn append(&self, record: &JobRecord);
    fn epoch_root(&self) -> Hash32;
    fn prove_inclusion(&self, job: &JobId) -> Option<InclusionProof>;
}
