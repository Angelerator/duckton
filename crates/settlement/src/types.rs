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

    /// Parse a user-friendly **base64** TON address (`EQ…`/`UQ…`/`kQ…`/`0Q…`),
    /// accepting both the url-safe (`-`/`_`) and standard (`+`/`/`) alphabets.
    ///
    /// The decoded 36-byte payload is `tag(1) ‖ workchain(i8) ‖ account(32) ‖
    /// crc16(2)`; the CRC16-CCITT (XMODEM) checksum is verified so a typo'd
    /// address is rejected rather than silently mis-resolved. The bounceable /
    /// testnet tag bits do not affect the resolved `(workchain, account)`.
    pub fn from_base64_str(s: &str) -> Result<Self, BindingError> {
        let bytes = b64_decode(s.trim()).ok_or(BindingError::BadAddress)?;
        if bytes.len() != 36 {
            return Err(BindingError::BadAddress);
        }
        let want = u16::from_be_bytes([bytes[34], bytes[35]]);
        if crc16_ccitt(&bytes[..34]) != want {
            return Err(BindingError::BadAddress);
        }
        let workchain = (bytes[1] as i8) as i32;
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&bytes[2..34]);
        Ok(Self { workchain, hash })
    }

    /// Parse either the raw `workchain:hex64` form (`0:abcd…`) or the
    /// user-friendly base64 form (`kQ…`/`EQ…`). This is the single entry point
    /// the live wiring uses so config / SQL can register addresses in whichever
    /// form the explorer / faucet / Acton printed.
    pub fn from_any_str(s: &str) -> Result<Self, BindingError> {
        let t = s.trim();
        if t.contains(':') {
            Self::from_raw_str(t)
        } else {
            Self::from_base64_str(t)
        }
    }
}

/// Decode standard or url-safe base64 (padding optional). Returns `None` on any
/// invalid character so callers can treat it as a malformed address.
fn b64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' | b'-' => Some(62),
            b'/' | b'_' => Some(63),
            _ => None,
        }
    }
    let s = s.trim_end_matches('=');
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u32;
    for &c in s.as_bytes() {
        let v = val(c)? as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

/// CRC16-CCITT (XMODEM: poly 0x1021, init 0x0000) — the checksum TON uses in the
/// user-friendly address encoding.
fn crc16_ccitt(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// Reasons a node's stake can be slashed (BLOCKCHAIN_ECONOMICS §8.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlashReason {
    WrongResult,
    Cheat,
    Downtime,
    Equivocation,
    AnchorFault,
    /// A **broken commitment**: the provider accepted/bid on a PAID job then
    /// failed to deliver a valid result by the deadline (no result / timeout /
    /// abandoned / wrong hash) WHILE the job was demonstrably feasible (a quorum
    /// was reached, or another selected node delivered a valid result). Fines the
    /// broken commitment to paid work — distinct from a wrong-result slash.
    FailedCommitment,
}

impl SlashReason {
    /// Discriminant matching the on-chain `StakeSlash.reason` byte. The on-chain
    /// `StakeVault` slash handler applies the same split for any reason byte, so
    /// a new reason needs no contract change — only a distinct discriminant.
    pub fn code(self) -> u8 {
        match self {
            SlashReason::WrongResult => 1,
            SlashReason::Cheat => 2,
            SlashReason::Downtime => 3,
            SlashReason::Equivocation => 4,
            SlashReason::AnchorFault => 5,
            SlashReason::FailedCommitment => 6,
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
    /// The on-chain `GlobalParams` version/seqno in force when the job ran. Bound
    /// into the anchored record (folded into its Merkle leaf) so settlement and
    /// dispute resolution reference the EXACT ecosystem params that governed the
    /// job — all parties (host, requester, coordinator) agree. `0` = unbound
    /// (free/legacy jobs, or no `GlobalParams` wired); fully additive.
    pub params_version: u32,
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
        // Bind the on-chain params version into the leaf (appended last so a
        // `params_version == 0` record matches the legacy encoding only in value,
        // not length — disputes commit to the exact params set in force).
        out.extend_from_slice(&self.params_version.to_le_bytes());
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

#[cfg(test)]
mod record_tests {
    use super::JobRecord;

    fn record(params_version: u32) -> JobRecord {
        JobRecord {
            job_id: "job-1".into(),
            query_hash: "qh".into(),
            requester_wallet: "0:00".into(),
            max_bid: 1_000,
            result_hash: "rh".into(),
            epoch: 3,
            prev_root: [0u8; 32],
            params_version,
        }
    }

    #[test]
    fn params_version_is_bound_into_the_record_leaf() {
        // Two records identical except for the bound params version must hash to
        // DIFFERENT leaves — so the anchored record commits to the exact params
        // set in force and a dispute can't be replayed under different policy.
        let a = record(1);
        let b = record(2);
        assert_ne!(a.canonical_bytes(), b.canonical_bytes());
        assert_ne!(a.leaf(), b.leaf());
        // Deterministic for a fixed version.
        assert_eq!(record(1).leaf(), record(1).leaf());
        // The version is the trailing 4 bytes (LE) of the canonical encoding.
        let bytes = record(0x0102_0304).canonical_bytes();
        let tail = &bytes[bytes.len() - 4..];
        assert_eq!(tail, &0x0102_0304u32.to_le_bytes());
    }
}

#[cfg(test)]
mod addr_tests {
    use super::WalletAddress;

    #[test]
    fn base64_user_friendly_matches_raw_form() {
        // Real testnet addresses + their canonical raw forms (toncenter
        // detectAddress). The url-safe base64 (`kQ…`, note the `-`/`_`) must
        // decode to the SAME (workchain, account) as the `0:hex` raw form.
        let cases = [
            (
                "kQCP7UqEfNwpaaNGDP3MihPPBb-Yd5ZYc0EU-VbXcmjpg422",
                "0:8fed4a847cdc2969a3460cfdcc8a13cf05bf98779658734114f956d77268e983",
            ),
            (
                "kQDBwfWwUy7EXuukEb5QCsrUme0Ri2XndhuPs0Lozb5TrrXx",
                "0:c1c1f5b0532ec45eeba411be500acad499ed118b65e7761b8fb342e8cdbe53ae",
            ),
        ];
        for (friendly, raw) in cases {
            let a = WalletAddress::from_base64_str(friendly).expect("base64 parses");
            let b = WalletAddress::from_raw_str(raw).expect("raw parses");
            assert_eq!(a, b, "base64 {friendly} must equal raw {raw}");
            assert_eq!(a.workchain, 0);
            // from_any_str dispatches on the `:` separator.
            assert_eq!(WalletAddress::from_any_str(friendly).unwrap(), a);
            assert_eq!(WalletAddress::from_any_str(raw).unwrap(), b);
        }
    }

    #[test]
    fn base64_rejects_crc_typo() {
        // Flip one payload char: the CRC16 check must reject it.
        let bad = "kQCP7UqEfNwpaaNGDP3MihPPBb-Yd5ZYc0EU-VbXcmjpg423";
        assert!(WalletAddress::from_base64_str(bad).is_err());
        // Garbage / wrong length is rejected too.
        assert!(WalletAddress::from_base64_str("not-an-address").is_err());
        assert!(WalletAddress::from_any_str("0:zz").is_err());
    }
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
    #[error("ton_proof domain does not match the expected app domain")]
    DomainMismatch,
    #[error("ton_proof timestamp is stale or too far in the future")]
    ProofExpired,
    #[error("wallet public key does not derive the claimed wallet address")]
    WalletAddressMismatch,
}
