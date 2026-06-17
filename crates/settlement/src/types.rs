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
    /// The QUOTED winning price — the fee base. The platform fee is φ·`base`
    /// (enforced on-chain) REGARDLESS of what the winner is actually paid. For a
    /// wallet winner `base == winner.amount`; for a free (walletless) winner
    /// `base > 0` while `winner.amount == 0` (the base refunds to the requester).
    pub base: Amount,
    /// Winner payout: `base` for a wallet winner, `0` for a free winner.
    pub winner: Payout,
    /// Fixed participation commission to each agreeing WALLET-holding non-winner.
    pub participants: Vec<Payout>,
    /// Platform fee (= φ·`base`) to the configured treasury / fee-recipient.
    pub platform_fee: Amount,
}

impl SettlementOutcome {
    /// Total funds directed OUT of escrow to payees (winner payout + commissions +
    /// fee). NOTE: this is the actual outflow bounded by `B` — for a free winner
    /// `winner.amount == 0`, so the quoted `base` is NOT counted here (it refunds
    /// to the requester). The off-chain coverage preflight separately requires
    /// `B >= base + fee + commissions` so the base can always be paid or refunded.
    pub fn total(&self) -> Amount {
        self.winner.amount
            + self.platform_fee
            + self.participants.iter().map(|p| p.amount).sum::<Amount>()
    }
}

/// Basis-points denominator (φ / κ are expressed in bps, /10000).
pub const BPS_DENOM: Amount = 10_000;

/// Worst-case settlement total the requester's escrow MUST cover BEFORE a paid job
/// runs: the winner's settled price `base` + the platform fee
/// (`base * platform_fee_bps / 10000`) + a participation commission
/// (`base * participation_commission_bps / 10000`) to EACH of the
/// `agreeing_non_winners` runners that ran the query and verified the result.
///
/// Integer **floor** arithmetic matches the on-chain `JobEscrow` settle math
/// byte-for-byte (the contract enforces `platformFee == winnerAmount*φ` and
/// `winner + fee + Σcommissions <= escrowAmount`), so this off-chain preflight and
/// the on-chain coverage bound agree exactly. NOTE: the per-contract storage
/// reserve (`MIN_TONS_FOR_STORAGE`) and the deploy/settle gas buffer are deliberately
/// NOT included — they are operational headroom funded separately (on top of `B`)
/// and are never promised to the paid parties, so the math is about the actual
/// DISTRIBUTABLE escrow.
pub fn required_escrow_total(
    base: Amount,
    agreeing_non_winners: usize,
    platform_fee_bps: u16,
    participation_commission_bps: u16,
) -> Amount {
    let fee = base.saturating_mul(platform_fee_bps as Amount) / BPS_DENOM;
    let commission_each = base.saturating_mul(participation_commission_bps as Amount) / BPS_DENOM;
    let commissions = commission_each.saturating_mul(agreeing_non_winners as Amount);
    base.saturating_add(fee).saturating_add(commissions)
}

/// Up-front coverage guard: REQUIRE the requester's available escrow / wallet
/// balance to cover the FULL multi-party settlement (winner base + φ platform fee +
/// κ commission × N agreeing runners). Returns the required total on success; on
/// failure a [`SettleError::InsufficientEscrow`] whose `Display` is the
/// human-readable "insufficient escrow: need X TON to cover winner + φ% platform
/// fee + κ% commission × N runners, have Y TON". Callers reject the job HERE rather
/// than silently under-paying or letting settlement fail mid-flight.
pub fn ensure_escrow_covers(
    available: Amount,
    base: Amount,
    agreeing_non_winners: usize,
    platform_fee_bps: u16,
    participation_commission_bps: u16,
) -> Result<Amount, SettleError> {
    let needed = required_escrow_total(
        base,
        agreeing_non_winners,
        platform_fee_bps,
        participation_commission_bps,
    );
    if available < needed {
        return Err(SettleError::InsufficientEscrow {
            needed,
            have: available,
            runners: agreeing_non_winners,
            platform_fee_bps,
            participation_commission_bps,
        });
    }
    Ok(needed)
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
    /// The input-snapshot fingerprint the verified result was computed over
    /// (deterministic-input verification). Bound into the anchored leaf so a
    /// dispute references the EXACT external inputs, not just the query text and
    /// answer. Empty (default / unpinned or legacy job) ⇒ unbound; fully additive.
    #[serde(default)]
    pub input_fingerprint: String,
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
        // Bind the input fingerprint (deterministic-input verification) last, so
        // an empty fingerprint is value-compatible with legacy leaves.
        field(self.input_fingerprint.as_bytes(), &mut out);
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
    /// Up-front coverage failure: the requester's escrow / wallet cannot cover the
    /// FULL multi-party settlement (winner base + φ platform fee + κ commission per
    /// agreeing runner). The job must be rejected before it runs (no partial pay).
    /// Amounts are nanoton; the message renders them in TON and the rates as %.
    #[error(
        "insufficient escrow: need {} TON to cover winner + {}% platform fee + {}% commission × {runners} runners, have {} TON",
        *needed as f64 / 1e9, *platform_fee_bps as f64 / 100.0, *participation_commission_bps as f64 / 100.0, *have as f64 / 1e9
    )]
    InsufficientEscrow {
        needed: Amount,
        have: Amount,
        runners: usize,
        platform_fee_bps: u16,
        participation_commission_bps: u16,
    },
    #[error("result hash does not match the escrow lock")]
    HashMismatch,
    /// FEE-RECIPIENT enforcement: the platform-fee recipient (`treasury`) bound (or
    /// configured) for a per-job escrow does NOT match the authoritative on-chain
    /// `GlobalParams.fee_recipient` for the pinned `params_version`. An honest
    /// requester/coordinator refuses to open or settle such an escrow so the admin
    /// treasury can never be silently replaced by a local-config value. Addresses
    /// are the raw `workchain:hex` form so the rejection is observable in logs.
    #[error(
        "fee recipient mismatch: escrow treasury {bound} != on-chain GlobalParams.fee_recipient {expected} (params_version {params_version})"
    )]
    TreasuryMismatch {
        bound: String,
        expected: String,
        params_version: u32,
    },
    #[error("settlement backend error: {0}")]
    Backend(String),
    #[error("settlement is disabled (free job): {0}")]
    Disabled(String),
    /// The message reached the chain but the destination contract's transaction
    /// FAILED (compute or action phase): non-zero exit/result code or aborted.
    #[error("on-chain transaction failed (compute/action exit code {exit_code})")]
    TxFailed { exit_code: i32 },
    /// No successful destination transaction was observed within the deadline —
    /// the external message may have been dropped (bad sig / replay / expired /
    /// under-funded) or is still in flight. The caller must NOT assume it landed.
    #[error("on-chain transaction not confirmed within the deadline")]
    TxUnconfirmed,
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
mod cost_model_tests {
    use super::{ensure_escrow_covers, required_escrow_total, SettleError};

    const TON: u128 = 1_000_000_000;

    #[test]
    fn multi_party_total_at_canonical_rates_is_exact() {
        // φ = 15% (1500 bps), κ = 5% (500 bps), winner base 80 TON, 2 agreeing
        // runners: 80 + 80*15% + 2*(80*5%) = 80 + 12 + 8 = 100 TON.
        let total = required_escrow_total(80 * TON, 2, 1500, 500);
        assert_eq!(total, 100 * TON);
        // No runners ⇒ just winner + the 15% fee.
        assert_eq!(required_escrow_total(80 * TON, 0, 1500, 500), 92 * TON);
    }

    #[test]
    fn coverage_accepts_when_escrow_fits_all_parties() {
        // Escrow exactly equal to the multi-party total is accepted (boundary).
        let needed = ensure_escrow_covers(100 * TON, 80 * TON, 2, 1500, 500).unwrap();
        assert_eq!(needed, 100 * TON);
        // A larger escrow is also fine.
        assert!(ensure_escrow_covers(120 * TON, 80 * TON, 2, 1500, 500).is_ok());
    }

    #[test]
    fn coverage_rejects_escrow_too_small_for_all_parties_up_front() {
        // 99 TON cannot cover winner 80 + 15% (12) + 5%×2 (8) = 100 TON.
        let err = ensure_escrow_covers(99 * TON, 80 * TON, 2, 1500, 500).unwrap_err();
        match err {
            SettleError::InsufficientEscrow {
                needed,
                have,
                runners,
                platform_fee_bps,
                participation_commission_bps,
            } => {
                assert_eq!(needed, 100 * TON);
                assert_eq!(have, 99 * TON);
                assert_eq!(runners, 2);
                assert_eq!(platform_fee_bps, 1500);
                assert_eq!(participation_commission_bps, 500);
            }
            other => panic!("expected InsufficientEscrow, got {other:?}"),
        }
        // The Display is the human-readable up-front rejection message.
        let msg = ensure_escrow_covers(99 * TON, 80 * TON, 2, 1500, 500)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("insufficient escrow"), "got: {msg}");
        assert!(msg.contains("15% platform fee"), "got: {msg}");
        assert!(msg.contains("5% commission"), "got: {msg}");
        assert!(msg.contains("2 runners"), "got: {msg}");
    }

    #[test]
    fn coverage_matches_floor_arithmetic_of_the_contract() {
        // base 90 → fee floor(90*15%) = 13.5 → 13 ... but integer bps math is on
        // nanoton, so 90 TON * 1500 / 10000 = 13.5 TON exactly (no truncation at
        // nanoton granularity); use an odd base to exercise the floor.
        let base = 7 * TON + 3; // 7.000000003 TON
        let total = required_escrow_total(base, 1, 1500, 500);
        let fee = base * 1500 / 10_000;
        let comm = base * 500 / 10_000;
        assert_eq!(total, base + fee + comm);
    }
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
            input_fingerprint: String::new(),
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
        // The version precedes the trailing length-prefixed input_fingerprint
        // field (empty here ⇒ a 4-byte length of 0), so it occupies the 4 bytes
        // just before that 4-byte length suffix.
        let bytes = record(0x0102_0304).canonical_bytes();
        assert_eq!(&bytes[bytes.len() - 4..], &0u32.to_le_bytes(), "empty fingerprint length");
        let ver = &bytes[bytes.len() - 8..bytes.len() - 4];
        assert_eq!(ver, &0x0102_0304u32.to_le_bytes());
    }

    #[test]
    fn input_fingerprint_is_bound_into_the_record_leaf() {
        // Two records identical except for the input fingerprint must hash to
        // different leaves (deterministic-input verification: the anchored record
        // commits to the exact inputs the verified result was computed over).
        let a = JobRecord {
            input_fingerprint: "fp-A".into(),
            ..record(1)
        };
        let b = JobRecord {
            input_fingerprint: "fp-B".into(),
            ..record(1)
        };
        assert_ne!(a.canonical_bytes(), b.canonical_bytes());
        assert_ne!(a.leaf(), b.leaf());
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
