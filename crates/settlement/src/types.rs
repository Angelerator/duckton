//! Shared types for the settlement layer (BLOCKCHAIN_ECONOMICS §10.1).

use serde::{Deserialize, Serialize};

use p2p_proto::JobId;

/// An amount of TON, denominated in nanoton (1 TON = 1e9 nanoton).
pub type Amount = u128;

/// A 32-byte Merkle root / hash.
pub type Hash32 = [u8; 32];

/// A TON wallet address (`workchain:hash`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WalletAddress {
    pub workchain: i32,
    pub hash: Hash32,
}

impl WalletAddress {
    pub fn new(workchain: i32, hash: Hash32) -> Self {
        Self { workchain, hash }
    }

    /// Raw on-chain encoding used by `ton_proof`: workchain as int32 big-endian
    /// (4 bytes) followed by the 32-byte account id.
    pub fn to_raw_bytes(&self) -> [u8; 36] {
        let mut out = [0u8; 36];
        out[0..4].copy_from_slice(&self.workchain.to_be_bytes());
        out[4..36].copy_from_slice(&self.hash);
        out
    }

    /// Parse the raw `"workchain:hex64"` form (e.g. `"0:abcd...."`).
    pub fn from_raw_str(s: &str) -> Result<Self, BindingError> {
        let (wc, hexpart) = s.split_once(':').ok_or(BindingError::BadAddress)?;
        let workchain: i32 = wc.parse().map_err(|_| BindingError::BadAddress)?;
        let bytes = hex::decode(hexpart).map_err(|_| BindingError::BadAddress)?;
        if bytes.len() != 32 {
            return Err(BindingError::BadAddress);
        }
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&bytes);
        Ok(Self { workchain, hash })
    }

    pub fn to_raw_string(&self) -> String {
        format!("{}:{}", self.workchain, hex::encode(self.hash))
    }
}

/// Reasons a node's stake can be slashed (BLOCKCHAIN_ECONOMICS §8.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlashReason {
    WrongResult,
    Cheat,
    Downtime,
    Equivocation,
    AnchorFault,
}

impl SlashReason {
    /// Discriminant matching the on-chain `StakeSlash.reason` byte.
    pub fn code(self) -> u8 {
        match self {
            SlashReason::WrongResult => 1,
            SlashReason::Cheat => 2,
            SlashReason::Downtime => 3,
            SlashReason::Equivocation => 4,
            SlashReason::AnchorFault => 5,
        }
    }
}

/// Handle to an opened escrow (the per-job `JobEscrow` contract address + B).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EscrowHandle {
    pub job: JobId,
    pub address: WalletAddress,
    pub max_bid: Amount,
}

/// A single payout in a settlement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Payout {
    pub to: WalletAddress,
    pub amount: Amount,
}

/// The outcome the coordinator hands to [`crate::Settlement::settle`] after the
/// quorum verdict (§6.2). All amounts are bounded by the escrowed `B`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettlementOutcome {
    /// The agreed quorum result hash (the HTLC key).
    pub result_hash: Hash32,
    /// Winner (fastest agreeing) base + perf bonus.
    pub winner: Payout,
    /// Fixed participation commission to each agreeing non-winner.
    pub participants: Vec<Payout>,
    /// Platform fee to the configured treasury / fee-recipient.
    pub platform_fee: Amount,
}

impl SettlementOutcome {
    /// Total funds directed out of escrow (winner + commissions + fee).
    pub fn total(&self) -> Amount {
        self.winner.amount
            + self.platform_fee
            + self.participants.iter().map(|p| p.amount).sum::<Amount>()
    }
}

/// Two-way wallet <-> node identity binding (BLOCKCHAIN_ECONOMICS §3.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeWalletBinding {
    /// `b3:...` node id (must equal BLAKE3(node_pubkey)).
    pub node_id: String,
    pub wallet_address: WalletAddress,
    /// Node identity Ed25519 public key (32 bytes).
    pub node_pubkey: [u8; 32],
    /// Wallet Ed25519 public key (32 bytes, from the wallet StateInit).
    pub wallet_pubkey: [u8; 32],
    /// Requester-issued nonce (replay protection).
    pub nonce: Vec<u8>,
    /// Expiry (unix seconds) for direction 1.
    pub expiry: u64,
    /// Direction 1: node attests the wallet (Ed25519 over the bind message).
    pub sig_node: Vec<u8>,
    /// Direction 2: wallet attests the node via `ton_proof` (ton-proof-item-v2).
    pub ton_proof: TonProof,
}

/// A `ton-proof-item-v2` signed payload (TON Connect).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TonProof {
    /// App domain (e.g. "duckdb-p2p").
    pub domain: String,
    /// Proof timestamp (unix seconds).
    pub timestamp: u64,
    /// Payload bytes — here `node_id ‖ nonce` (§3.1).
    pub payload: Vec<u8>,
    /// Ed25519 signature (64 bytes) by the wallet key.
    pub signature: Vec<u8>,
}

/// A Merkle inclusion proof against an anchored epoch root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InclusionProof {
    pub leaf: Hash32,
    /// Ordered sibling path: `(sibling_is_left, sibling_hash)` from leaf to root.
    pub siblings: Vec<(bool, Hash32)>,
}

/// An off-chain job record (BLOCKCHAIN_ECONOMICS §7.2). Its BLAKE3 leaf is
/// batched into the per-epoch Merkle tree whose root is anchored on-chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobRecord {
    pub job_id: String,
    pub query_hash: String,
    pub requester_wallet: String,
    pub max_bid: Amount,
    pub result_hash: String,
    pub epoch: u64,
    pub prev_root: Hash32,
}

impl JobRecord {
    /// Deterministic canonical encoding (length-prefixed fields).
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let field = |b: &[u8], out: &mut Vec<u8>| {
            out.extend_from_slice(&(b.len() as u32).to_le_bytes());
            out.extend_from_slice(b);
        };
        field(self.job_id.as_bytes(), &mut out);
        field(self.query_hash.as_bytes(), &mut out);
        field(self.requester_wallet.as_bytes(), &mut out);
        out.extend_from_slice(&self.max_bid.to_le_bytes());
        field(self.result_hash.as_bytes(), &mut out);
        out.extend_from_slice(&self.epoch.to_le_bytes());
        out.extend_from_slice(&self.prev_root);
        out
    }

    /// The Merkle leaf for this record: `BLAKE3(canonical(record))` (§7.2).
    pub fn leaf(&self) -> Hash32 {
        *blake3::hash(&self.canonical_bytes()).as_bytes()
    }
}

/// Errors from the money rail.
#[derive(Debug, thiserror::Error)]
pub enum SettleError {
    #[error("escrow not funded for job {0}")]
    NotFunded(String),
    #[error("payout {payout} exceeds escrow {escrow}")]
    PayoutExceedsEscrow { payout: Amount, escrow: Amount },
    #[error("result hash does not match the escrow lock")]
    HashMismatch,
    #[error("settlement backend error: {0}")]
    Backend(String),
    #[error("settlement is disabled (free job): {0}")]
    Disabled(String),
}

/// Errors from the stake registry.
#[derive(Debug, thiserror::Error)]
pub enum SlashError {
    #[error("unknown node {0}")]
    UnknownNode(String),
    #[error("amount {amount} exceeds slashable stake {slashable}")]
    ExceedsStake { amount: Amount, slashable: Amount },
    #[error("stake backend error: {0}")]
    Backend(String),
}

/// Errors from wallet binding / ton_proof verification.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BindingError {
    #[error("malformed wallet address")]
    BadAddress,
    #[error("malformed key or signature")]
    BadKeyOrSig,
    #[error("node id does not match node public key")]
    NodeIdMismatch,
    #[error("direction-1 (node attests wallet) signature invalid")]
    NodeSigInvalid,
    #[error("direction-2 (ton_proof) signature invalid")]
    TonProofInvalid,
    #[error("binding expired")]
    Expired,
    #[error("ton_proof payload does not bind this node/nonce")]
    PayloadMismatch,
}
