//! TON-backed implementations that talk to the on-chain contracts in `ton/`.
//!
//! Honesty note: live testnet/mainnet calls cannot be exercised in this
//! environment, so this module is structured so that everything *except* the
//! network hop is unit-tested:
//!   * message **opcodes** match the Tolk contracts (`ton/contracts/*`),
//!   * message **body serialization** (the ABI field order) is built and tested
//!     here,
//!   * the network transport is abstracted behind [`TonRpc`]; a real HTTP
//!     (toncenter) client is gated behind the `ton-live` feature, and the
//!     default [`NullTonRpc`] returns a typed "disabled" error.
//!
//! The body byte layout below is a stable internal ABI used for unit tests; the
//! live client (feature `ton-live`) maps these typed fields onto proper TL-B
//! cells / BoC before broadcasting.

use std::collections::HashMap;

use p2p_config::{DataClassCfg, EconomicsConfig, SchedulerConfig};
use p2p_proto::{JobId, NodeId};

use crate::cell::{
    address_key_bits, state_init_cell, Cell, CellBuilder, DictEntry, ADDRESS_KEY_BITS, BASECHAIN,
};
use crate::traits::{RecordAnchor, Settlement, StakeRegistry};
use crate::types::{
    Amount, EscrowHandle, Hash32, InclusionProof, JobRecord, Payout, SettleError,
    SettlementOutcome, SlashError, SlashReason, WalletAddress,
};

// Opcodes — MUST match `ton/contracts/*` (see stake_types.tolk / escrow_types.tolk / anchor_types.tolk).
pub const OP_STAKE_DEPOSIT: u32 = 0x534b_4b01;
pub const OP_STAKE_UNBOND: u32 = 0x534b_4b02;
pub const OP_STAKE_WITHDRAW: u32 = 0x534b_4b03;
pub const OP_STAKE_SLASH: u32 = 0x534b_4b05;
// StakeVault timelocked governance code upgrade (§8.6) — MUST match `stake_types.tolk`.
pub const OP_STAKE_ANNOUNCE_UPGRADE: u32 = 0x534b_4b07;
pub const OP_STAKE_APPLY_UPGRADE: u32 = 0x534b_4b08;
pub const OP_STAKE_CANCEL_UPGRADE: u32 = 0x534b_4b09;
// StakeVault bounce-safe pull-withdrawal (C1) — MUST match `stake_types.tolk`.
pub const OP_STAKE_CLAIM: u32 = 0x534b_4b0a;
/// Outgoing payout body tag carried on every pushed StakeVault payout (C1) so a
/// bounce is identifiable in `onBouncedMessage`. Not a message the bridge sends;
/// exported for parity with the contract.
pub const OP_STAKE_PAYOUT: u32 = 0x534b_4b0b;
pub const OP_ESCROW_TOPUP: u32 = 0x4553_4300;
pub const OP_ESCROW_SETTLE: u32 = 0x4553_4302;
pub const OP_ESCROW_REFUND: u32 = 0x4553_4303;
// JobEscrow bounce-safe pull-withdrawal (B2) — MUST match `escrow_types.tolk`.
pub const OP_ESCROW_CLAIM: u32 = 0x4553_4304;
/// Outgoing payout body tag carried on every pushed escrow payout (B2). Not a
/// message the bridge sends; exported for parity with the contract.
pub const OP_ESCROW_PAYOUT: u32 = 0x4553_4305;
pub const OP_ANCHOR_SUBMIT: u32 = 0x414e_4301;
// RecordAnchor bonded dispute (proof verified against the STORED epoch root).
pub const OP_ANCHOR_OPEN_DISPUTE: u32 = 0x414e_4302;
// RecordAnchor authority-gated in-place code upgrade — MUST match `anchor_types.tolk`.
pub const OP_ANCHOR_UPGRADE_CODE: u32 = 0x414e_4304;
/// Outgoing bond-refund body tag (A1 bounce-safety) carried on every RecordAnchor
/// bond refund. Not a message the bridge sends; exported for parity.
pub const OP_ANCHOR_BOND_REFUND: u32 = 0x414e_4305;
// GlobalParams (platform-wide economic params) — MUST match `global_params_types.tolk`.
pub const OP_UPDATE_PARAMS: u32 = 0x4750_4101;
pub const OP_UPDATE_ADMIN: u32 = 0x4750_4102;
// GlobalParams admin-gated in-place code upgrade (§12.1), now TIMELOCKED: it
// requires a prior `AnnounceCode` + an elapsed `upgradeDelay` (step 2 = apply).
pub const OP_UPGRADE_CODE: u32 = 0x4750_4104;
// GlobalParams TIMELOCKED code upgrade — step 1 (announce) / safety-valve cancel.
pub const OP_ANNOUNCE_CODE: u32 = 0x4750_4105;
pub const OP_CANCEL_CODE: u32 = 0x4750_4106;

/// A monotonic, per-client `queryId` source. Every internal message a contract
/// processes carries a 64-bit `queryId` (its standard TL-B reply-correlation
/// field); broadcasting many messages all stamped `0` makes them
/// indistinguishable in explorers / bounce handling, so each TON client owns one
/// of these and stamps a fresh value per message it sends.
///
/// It is a process-local counter, NOT a wall clock: `Date::now`-style sources are
/// forbidden in parts of this crate (and would make tests non-deterministic), so
/// this seeds at `1` and increments. Determinism: a freshly constructed client
/// emits `1, 2, 3, …`, so a test that constructs the client and drives a known
/// sequence of sends sees a known sequence of `queryId`s.
#[derive(Debug, Default)]
pub struct QueryIdGen(std::sync::atomic::AtomicU64);

impl QueryIdGen {
    /// A generator that emits `1, 2, 3, …`.
    pub fn new() -> Self {
        Self(std::sync::atomic::AtomicU64::new(1))
    }
    /// The next unique `queryId` (wraps astronomically far in the future; a 64-bit
    /// counter at any realistic send rate never repeats within a deployment).
    pub fn next(&self) -> u64 {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }
}

/// A typed message body for an on-chain contract.
///
/// It carries TWO synchronized representations:
///   * `bytes` — a stable, deterministic flat ABI used by the unit tests (fixed
///     16-byte coins / 36-byte addresses) to pin field order against the Tolk
///     contracts,
///   * `cell` — the **real** TL-B body cell (`coins` as VarUInteger16,
///     `MsgAddressInt`, child refs, dicts) that the live broadcaster
///     ([`crate::wallet`]) wraps into a signed wallet-v5r1 external message.
///
/// Both are produced field-for-field by the same `build_*` functions, so the
/// flat ABI tests double as a layout check on the live cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageBody {
    pub opcode: u32,
    pub bytes: Vec<u8>,
    /// The on-chain TL-B body cell (what actually gets broadcast).
    pub cell: Cell,
    /// Internal cell builder kept in lockstep with `bytes`.
    cb: CellBuilder,
}

impl MessageBody {
    fn new(opcode: u32) -> Self {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&opcode.to_be_bytes());
        Self {
            opcode,
            bytes,
            cell: Cell::default(),
            cb: CellBuilder::new().store_uint(opcode as u128, 32),
        }
    }
    fn u64(mut self, v: u64) -> Self {
        self.bytes.extend_from_slice(&v.to_be_bytes());
        self.cb = self.cb.store_uint(v as u128, 64);
        self
    }
    fn u32(mut self, v: u32) -> Self {
        self.bytes.extend_from_slice(&v.to_be_bytes());
        self.cb = self.cb.store_uint(v as u128, 32);
        self
    }
    fn coins(mut self, v: Amount) -> Self {
        self.bytes.extend_from_slice(&v.to_be_bytes());
        self.cb = self.cb.store_coins(v);
        self
    }
    fn addr(mut self, a: &WalletAddress) -> Self {
        self.bytes.extend_from_slice(&a.to_raw_bytes());
        self.cb = self.cb.store_address(a);
        self
    }
    fn hash(mut self, h: &[u8; 32]) -> Self {
        self.bytes.extend_from_slice(h);
        self.cb = self.cb.store_u256(h);
        self
    }
    fn byte(mut self, b: u8) -> Self {
        self.bytes.push(b);
        self.cb = self.cb.store_uint(b as u128, 8);
        self
    }
    /// Append a raw child ref to the cell ONLY (the flat byte ABI keeps such a
    /// field inline; the live cell uses a `^ref`, e.g. `UpdateParams.params`).
    fn cell_ref(mut self, c: Cell) -> Self {
        self.cb = self.cb.store_ref(c);
        self
    }
    /// Append a TL-B `HashmapE key_bits X` (a Tolk `map<K, V>` field) to the live
    /// cell ONLY. The flat byte ABI omits dictionaries (like `cell_ref`), so the
    /// dict layout is pinned via the live cell's repr-hash in the tests.
    fn dict(mut self, key_bits: usize, entries: &[DictEntry]) -> Self {
        self.cb = self.cb.store_dict(key_bits, entries);
        self
    }
    /// Append a TL-B `Maybe ^Cell` (a `0` bit when `None`, else a `1` bit + a
    /// child ref) to the live cell ONLY — e.g. `AnchorOpenDispute.proof`, a
    /// `Maybe ^MerkleStep` chain. The flat byte ABI omits the optional ref.
    fn maybe_ref(mut self, cell: Option<Cell>) -> Self {
        self.cb = self.cb.store_maybe_ref(cell);
        self
    }
    /// Finalize: snapshot the accumulated cell builder into `cell`.
    fn finish(mut self) -> Self {
        self.cell = self.cb.clone().build();
        self
    }
}

/// Build the `StakeDeposit` body.
pub fn build_stake_deposit(query_id: u64, amount: Amount) -> MessageBody {
    MessageBody::new(OP_STAKE_DEPOSIT)
        .u64(query_id)
        .coins(amount)
        .finish()
}

/// Build the `StakeSlash` body.
pub fn build_stake_slash(
    query_id: u64,
    amount: Amount,
    reason: SlashReason,
    challenger: &WalletAddress,
) -> MessageBody {
    MessageBody::new(OP_STAKE_SLASH)
        .u64(query_id)
        .coins(amount)
        .byte(reason.code())
        .addr(challenger)
        .finish()
}

/// Build the `StakeRequestUnbond` body (`queryId, amount`).
pub fn build_stake_unbond(query_id: u64, amount: Amount) -> MessageBody {
    MessageBody::new(OP_STAKE_UNBOND)
        .u64(query_id)
        .coins(amount)
        .finish()
}

/// Build the `EscrowSettle` body (mirrors `escrow_types.tolk::EscrowSettle`). The
/// HTLC scalar fields are encoded inline; `participants: map<address, coins>` is
/// emitted as a real `HashmapE` (267-bit `addr_std` keys → commission `coins`)
/// that the contract iterates to pay each agreeing non-winner κ·payout_win. An
/// empty slice ⇒ the empty-dict `0` bit (winner + fee only, slack refunded).
///
/// B1: `candidates` is the requester-pre-committed payout-eligible set, emitted
/// as the trailing `map<address, uint1>` (267-bit `addr_std` keys → a 1-bit `1`).
/// The contract asserts `candidatesCommitment(candidates) == terms.candidatesHash`
/// AND that the `winner` + every `participants` key is a member, so it MUST be the
/// SAME set the escrow's terms committed to at open (see [`candidates_commitment`]
/// / [`build_escrow_terms`]). Duplicate addresses collapse (a map key is unique).
///
/// Participants sharing a payout wallet are merged (their commissions summed) so
/// the on-chain map — which cannot hold duplicate keys — stays well-formed.
pub fn build_escrow_settle(
    query_id: u64,
    result_hash: &[u8; 32],
    winner: &WalletAddress,
    base: Amount,
    winner_amount: Amount,
    platform_fee: Amount,
    participants: &[Payout],
    candidates: &[WalletAddress],
) -> MessageBody {
    let entries = participants_dict_entries(participants);
    let candidate_entries = candidates_dict_entries(candidates);
    MessageBody::new(OP_ESCROW_SETTLE)
        .u64(query_id)
        .hash(result_hash)
        .addr(winner)
        // FREE-NODE POLICY: `base` is the quoted price + the fee base (φ·base is
        // enforced on-chain regardless of `winner_amount`). `winner_amount` is the
        // actual payout: `base` for a wallet winner, `0` for a free winner (the
        // base then refunds to the requester on-chain).
        .coins(base)
        .coins(winner_amount)
        .coins(platform_fee)
        .dict(ADDRESS_KEY_BITS, &entries)
        .dict(ADDRESS_KEY_BITS, &candidate_entries)
        .finish()
}

/// Encode the requester-committed candidate set as TON dictionary entries
/// (`addr_std` key bits → a 1-bit `uint1` value of `1`), deduplicated and sorted
/// canonically by the dict builder. This is the `map<address, uint1>` the
/// contract iterates / membership-checks against.
fn candidates_dict_entries(candidates: &[WalletAddress]) -> Vec<DictEntry> {
    let mut seen: Vec<WalletAddress> = Vec::new();
    for c in candidates {
        if !seen.contains(c) {
            seen.push(*c);
        }
    }
    seen.into_iter()
        .map(|to| DictEntry {
            key: address_key_bits(&to),
            value: CellBuilder::new().store_uint(1, 1).build(),
        })
        .collect()
}

/// The requester's candidate-set commitment, byte-identical to the on-chain
/// `candidatesCommitment(candidates)` in `escrow_types.tolk`:
/// `cellhash( beginCell().storeDict(candidates).endCell() )` over the SAME
/// `map<address, uint1>` (267-bit `addr_std` keys → 1-bit `1`). This is the value
/// bound into [`build_escrow_terms`]'s `candidates_hash` at open, and re-presented
/// (as the `candidates` map) at settle. An empty set hashes the empty-dict `0`
/// bit. Cross-checked against the Acton emulator (`ton/scripts/_probe_v2.tolk`).
pub fn candidates_commitment(candidates: &[WalletAddress]) -> Hash32 {
    let entries = candidates_dict_entries(candidates);
    CellBuilder::new()
        .store_dict(ADDRESS_KEY_BITS, &entries)
        .build()
        .repr_hash()
}

/// Encode participant payouts as TON dictionary entries (`addr_std` key bits →
/// a `coins` value cell), merging any duplicate payout wallets by summing their
/// commissions (a `map` key is unique) and skipping zero-amount entries.
fn participants_dict_entries(participants: &[Payout]) -> Vec<DictEntry> {
    let mut merged: Vec<(WalletAddress, Amount)> = Vec::new();
    for p in participants {
        if p.amount == 0 {
            continue;
        }
        match merged.iter_mut().find(|(w, _)| *w == p.to) {
            Some((_, amt)) => *amt = amt.saturating_add(p.amount),
            None => merged.push((p.to, p.amount)),
        }
    }
    merged
        .into_iter()
        .map(|(to, amount)| DictEntry {
            key: address_key_bits(&to),
            value: CellBuilder::new().store_coins(amount).build(),
        })
        .collect()
}

/// Build the `EscrowRefund` body (`queryId`).
pub fn build_escrow_refund(query_id: u64) -> MessageBody {
    MessageBody::new(OP_ESCROW_REFUND).u64(query_id).finish()
}

/// Build the `EscrowClaim` body (`queryId, recipient`) — the B2 pull path that
/// re-delivers a queued/bounced payout to the `recipient` it is keyed under (the
/// escrow only ever sends the funds to that recipient, so anyone — e.g. a keeper —
/// may trigger it without being able to redirect a payout).
pub fn build_escrow_claim(query_id: u64, recipient: &WalletAddress) -> MessageBody {
    MessageBody::new(OP_ESCROW_CLAIM)
        .u64(query_id)
        .addr(recipient)
        .finish()
}

/// Build the `EscrowTopUp` body (`queryId`) — the message that funds a freshly
/// deployed per-job escrow with the locked bid `B`.
pub fn build_escrow_topup(query_id: u64) -> MessageBody {
    MessageBody::new(OP_ESCROW_TOPUP).u64(query_id).finish()
}

/// Current unix time in seconds (used for escrow deadlines / message expiry).
fn now_secs_u32() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0)
}

/// Build the `AnchorSubmitRoot` body.
pub fn build_anchor_submit(
    query_id: u64,
    epoch: u32,
    root: &[u8; 32],
    prev_root: &[u8; 32],
    stake_weight: Amount,
) -> MessageBody {
    MessageBody::new(OP_ANCHOR_SUBMIT)
        .u64(query_id)
        .u32(epoch)
        .hash(root)
        .hash(prev_root)
        .coins(stake_weight)
        .finish()
}

/// Build the `Cell<MerkleStep>?` proof chain (a `Maybe ^MerkleStep` linked list)
/// from an off-chain [`InclusionProof`]'s sibling path, mirroring
/// `anchor_types.tolk::MerkleStep { dir: uint1, sibling: uint256, next: Cell<MerkleStep>? }`.
///
/// `dir` encodes which side the sibling is on, matching the contract's fold and
/// the off-chain `verify_inclusion`: `dir = 1` when the sibling is the LEFT node
/// (`sibling_is_left == true`), `dir = 0` when it is the RIGHT node. The list is
/// nested innermost-last: the FIRST sibling (closest to the leaf) becomes the
/// outermost cell (the one the dispute body references), and the deepest `next` is
/// the absent ref (`None`). An empty path ⇒ `None` (a single-leaf tree where the
/// leaf already equals the root). The cell carries the RAW leaf value separately
/// (in the dispute body); the contract re-applies `hashLeaf` so a node value can
/// never masquerade as a leaf (the v2 second-preimage fix).
pub fn build_merkle_proof_cell(proof: &InclusionProof) -> Option<Cell> {
    let mut next: Option<Cell> = None;
    for (sibling_is_left, sibling) in proof.siblings.iter().rev() {
        let dir: u128 = if *sibling_is_left { 1 } else { 0 };
        next = Some(
            CellBuilder::new()
                .store_uint(dir, 1)
                .store_u256(sibling)
                .store_maybe_ref(next)
                .build(),
        );
    }
    next
}

/// Build the `AnchorOpenDispute` body (`queryId, epoch, leaf, proof`), mirroring
/// the hardened `anchor_types.tolk::AnchorOpenDispute`.
///
/// A2: `claimedRoot` is GONE — the contract verifies the inclusion proof against
/// the root it has STORED for `epoch` (`storage.roots[epoch]`), so neither the
/// root nor "which root" is attacker-controlled. The bridge therefore submits only
/// the contested `leaf` + the sibling `proof`; the chained `proof` cell is built by
/// [`build_merkle_proof_cell`] (a `Maybe ^MerkleStep`). `leaf` is the RAW
/// `JobRecord` leaf value (the contract re-applies `hashLeaf`). The challenger
/// bond travels as the message VALUE (`>= disputeBondMin`), not in the body.
pub fn build_anchor_open_dispute(
    query_id: u64,
    epoch: u32,
    leaf: &[u8; 32],
    proof: &InclusionProof,
) -> MessageBody {
    let proof_cell = build_merkle_proof_cell(proof);
    // The flat ABI keeps the scalar prefix inline; `proof` is a `Maybe ^MerkleStep`
    // (a `0` bit when absent, else a `1` bit + the chain ref) on the live cell.
    MessageBody::new(OP_ANCHOR_OPEN_DISPUTE)
        .u64(query_id)
        .u32(epoch)
        .hash(leaf)
        .maybe_ref(proof_cell)
        .finish()
}

/// Platform-wide economic parameters held by the on-chain `GlobalParams`
/// contract (BLOCKCHAIN_ECONOMICS §4–§8, §12), in their on-chain representation
/// (bps for fractions, nanoton for stakes, seconds for windows). The off-chain
/// node derives these from `[economics]` (a single source of truth) so newly
/// created escrow/stake instances are parameterized identically to what the
/// admin pushes on-chain, and the admin SQL setter can send `update_params`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlobalParams {
    pub platform_fee_bps: u16,
    pub surcharge_bps: u16,
    pub participation_commission_bps: u16,
    pub slash_wrong_bps: u16,
    pub slash_cheat_bps: u16,
    pub slash_downtime_bps: u16,
    pub slash_equivocation_bps: u16,
    pub split_challenger_bps: u16,
    pub split_redundancy_bps: u16,
    pub split_burn_bps: u16,
    pub split_treasury_bps: u16,
    pub min_stake: Amount,
    pub min_stake_internal: Amount,
    pub min_stake_sensitive: Amount,
    pub stake_cap: Amount,
    pub unbonding_secs: u32,
    pub challenge_window_secs: u32,
    pub n_public: u8,
    pub n_default: u8,
    pub n_max: u8,
    pub quorum: u8,
    pub checksum_min: u8,
    pub w_quality_bps: u16,
    pub w_stake_bps: u16,
    pub w_price_bps: u16,
    /// --- Resilience / fairness gating (ARCHITECTURE §8/§11) ---------------
    /// Failed-commitment slash (bps of bonded stake): the fine for accepting a
    /// PAID job and not delivering a valid result by the deadline while it was
    /// feasible (§8.3). On-chain so disputes reference one agreed value.
    pub slash_failed_commitment_bps: u16,
    /// Requester per-attempt deadline (ms): the boundary between an
    /// inconclusive attempt (job-fault, no penalty) and a broken commitment.
    pub attempt_deadline_ms: u32,
    /// Expected progress/heartbeat interval (ms) the host streams at.
    pub progress_interval_ms: u32,
    /// Stall-timeout multiplier (× `progress_interval_ms`) before an attempt is
    /// declared stalled.
    pub progress_stall_mult: u8,
}

impl GlobalParams {
    /// Validate the §12 invariants (mirrors `GlobalParams.tolk::validateEcoParams`
    /// and `EconomicsConfig::validate`), so the admin setter rejects bad params
    /// off-chain before paying gas for an on-chain rejection.
    pub fn validate(&self) -> Result<(), String> {
        let split = self.split_challenger_bps as u32
            + self.split_redundancy_bps as u32
            + self.split_burn_bps as u32
            + self.split_treasury_bps as u32;
        if split != 10_000 {
            return Err(format!("slash split bps must sum to 10000, got {split}"));
        }
        if self.participation_commission_bps > 1_000 {
            return Err("participation_commission_bps must be <= 1000 (10%)".into());
        }
        if !(self.min_stake_sensitive >= self.min_stake_internal
            && self.min_stake_internal >= self.min_stake)
        {
            return Err("require min_stake_sensitive >= min_stake_internal >= min_stake".into());
        }
        if self.stake_cap < self.min_stake {
            return Err("stake_cap must be >= min_stake".into());
        }
        if self.unbonding_secs < self.challenge_window_secs {
            return Err("unbonding_secs must be >= challenge_window_secs".into());
        }
        if self.checksum_min < 1
            || self.n_public < self.checksum_min
            || self.n_default < self.n_public
            || self.n_max < self.n_default
        {
            return Err("require n_max >= n_default >= n_public >= checksum_min >= 1".into());
        }
        if self.quorum < 1 || self.quorum > self.n_default {
            return Err("require 1 <= quorum <= n_default".into());
        }
        // Resilience gating mirrors `validateEcoParams`: the failed-commitment
        // slash is a stake fraction, and the stall timeout must be well-defined.
        if self.slash_failed_commitment_bps > 10_000 {
            return Err("slash_failed_commitment_bps must be <= 10000".into());
        }
        if self.progress_interval_ms < 1 {
            return Err("progress_interval_ms must be >= 1".into());
        }
        if self.progress_stall_mult < 1 {
            return Err("progress_stall_mult must be >= 1".into());
        }
        Ok(())
    }
}

/// Build the (scalar) `UpdateParams` body for `GlobalParams` (§12). The
/// `EcoParams` field order mirrors `global_params_types.tolk::EcoParams`; the
/// live BoC layer (feature `ton-live`) packs the params into the child cell ref
/// the contract expects.
pub fn build_update_params(
    query_id: u64,
    fee_recipient: &WalletAddress,
    p: &GlobalParams,
) -> MessageBody {
    // Flat ABI (unit-tested): all EcoParams fields inline. Live cell: the params
    // live in a `^EcoParams` child cell (`UpdateParams.params: Cell<EcoParams>`).
    let (flat, eco) = eco_params_encoded(p);
    let mut b = MessageBody::new(OP_UPDATE_PARAMS)
        .u64(query_id)
        .addr(fee_recipient);
    b.bytes.extend_from_slice(&flat); // flat ABI keeps the params inline
    b.cell_ref(eco).finish() // live cell references the child params cell
}

/// Encode `EcoParams` BOTH as the flat test ABI bytes AND as the on-chain child
/// cell (field-for-field identical order, mirroring `global_params_types.tolk`).
fn eco_params_encoded(p: &GlobalParams) -> (Vec<u8>, Cell) {
    let mut bytes = Vec::new();
    let mut cb = CellBuilder::new();
    macro_rules! u16f {
        ($v:expr) => {{
            bytes.extend_from_slice(&($v).to_be_bytes());
            cb = cb.store_uint($v as u128, 16);
        }};
    }
    macro_rules! coinsf {
        ($v:expr) => {{
            bytes.extend_from_slice(&($v).to_be_bytes());
            cb = cb.store_coins($v);
        }};
    }
    macro_rules! u32f {
        ($v:expr) => {{
            bytes.extend_from_slice(&($v).to_be_bytes());
            cb = cb.store_uint($v as u128, 32);
        }};
    }
    macro_rules! bytef {
        ($v:expr) => {{
            bytes.push($v);
            cb = cb.store_uint($v as u128, 8);
        }};
    }
    u16f!(p.platform_fee_bps);
    u16f!(p.surcharge_bps);
    u16f!(p.participation_commission_bps);
    u16f!(p.slash_wrong_bps);
    u16f!(p.slash_cheat_bps);
    u16f!(p.slash_downtime_bps);
    u16f!(p.slash_equivocation_bps);
    u16f!(p.split_challenger_bps);
    u16f!(p.split_redundancy_bps);
    u16f!(p.split_burn_bps);
    u16f!(p.split_treasury_bps);
    coinsf!(p.min_stake);
    coinsf!(p.min_stake_internal);
    coinsf!(p.min_stake_sensitive);
    coinsf!(p.stake_cap);
    u32f!(p.unbonding_secs);
    u32f!(p.challenge_window_secs);
    bytef!(p.n_public);
    bytef!(p.n_default);
    bytef!(p.n_max);
    bytef!(p.quorum);
    bytef!(p.checksum_min);
    u16f!(p.w_quality_bps);
    u16f!(p.w_stake_bps);
    u16f!(p.w_price_bps);
    // Resilience / fairness gating (appended last, mirroring the Tolk struct).
    u16f!(p.slash_failed_commitment_bps);
    u32f!(p.attempt_deadline_ms);
    u32f!(p.progress_interval_ms);
    bytef!(p.progress_stall_mult);
    (bytes, cb.build())
}

/// Build the `UpdateAdmin` body (admin rotation → multisig).
pub fn build_update_admin(query_id: u64, new_admin: &WalletAddress) -> MessageBody {
    MessageBody::new(OP_UPDATE_ADMIN)
        .u64(query_id)
        .addr(new_admin)
        .finish()
}

// ---------------------------------------------------------------------------
// In-place code-upgrade message builders (TVM SETCODE) — the address-stable
// upgradeability path (BLOCKCHAIN_ECONOMICS §8.6, §12.1). The new code travels
// as a `^ref` (a contract code cell is a multi-cell tree), so — like
// `build_update_params`'s EcoParams child — the flat test ABI keeps only the
// scalar prefix (opcode + queryId [+ hash]) inline and the live cell carries the
// code as a child ref. `ton-live` gating is unchanged: these builders are pure
// (no network); only the broadcaster (feature `ton-live`) hits the chain.
// ---------------------------------------------------------------------------

/// Build the GlobalParams `UpgradeCode` body (`queryId`, `newCode: ^cell`) — D2
/// step 2/2 (APPLY). The admin sends this to swap the contract CODE in place via
/// SETCODE — address unchanged, storage preserved, `codeVersion` bumped on-chain
/// (§12.1). The hardened contract now requires a PRIOR [`build_announce_code`]
/// with a MATCHING code hash AND an elapsed `upgradeDelay` (the timelock); a bare
/// apply (no announce / before the delay / hash mismatch) is rejected on-chain.
pub fn build_upgrade_code(query_id: u64, new_code: &Cell) -> MessageBody {
    MessageBody::new(OP_UPGRADE_CODE)
        .u64(query_id)
        .cell_ref(new_code.clone())
        .finish()
}

/// Build the GlobalParams `AnnounceCode` body (`queryId`, `newCodeHash`) — D2 step
/// 1/2 (ANNOUNCE). Admin-gated: commits to the successor code's 256-bit cell hash
/// and starts the `upgradeDelay` timelock clock; re-announcing overwrites and
/// RESETS the clock. Publicly observable via `get_pending_upgrade` so the
/// ecosystem can react before the apply lands.
pub fn build_announce_code(query_id: u64, new_code_hash: &[u8; 32]) -> MessageBody {
    MessageBody::new(OP_ANNOUNCE_CODE)
        .u64(query_id)
        .hash(new_code_hash)
        .finish()
}

/// Build the GlobalParams `CancelCode` body (`queryId`) — D2 admin-gated safety
/// valve to abort a pending code-upgrade announcement before it is applied.
pub fn build_cancel_code(query_id: u64) -> MessageBody {
    MessageBody::new(OP_CANCEL_CODE).u64(query_id).finish()
}

/// Build the RecordAnchor `AnchorUpgradeCode` body (`queryId`, `newCode: ^cell`).
/// The configured `verdictAuthority` sends this to swap the anchor CODE in place
/// (epoch chain + disputes preserved, address unchanged).
pub fn build_anchor_upgrade_code(query_id: u64, new_code: &Cell) -> MessageBody {
    MessageBody::new(OP_ANCHOR_UPGRADE_CODE)
        .u64(query_id)
        .cell_ref(new_code.clone())
        .finish()
}

/// Build the StakeVault `StakeAnnounceUpgrade` body (`queryId`, `newCodeHash`):
/// step 1 of the TIMELOCKED upgrade (§8.6). The governance authority commits to
/// the successor code's 256-bit cell hash and starts the timelock clock; apply
/// is rejected until `announce + unbondingPeriod` (the staker exit window).
pub fn build_stake_announce_upgrade(query_id: u64, new_code_hash: &[u8; 32]) -> MessageBody {
    MessageBody::new(OP_STAKE_ANNOUNCE_UPGRADE)
        .u64(query_id)
        .hash(new_code_hash)
        .finish()
}

/// Build the StakeVault `StakeApplyUpgrade` body (`queryId`, `newCode: ^cell`):
/// step 2 of the timelocked upgrade (§8.6). Accepted only after the timelock and
/// only if `newCode.hash() == pendingCodeHash` (binds apply to the announced
/// code); performs the in-place SETCODE preserving every bond.
pub fn build_stake_apply_upgrade(query_id: u64, new_code: &Cell) -> MessageBody {
    MessageBody::new(OP_STAKE_APPLY_UPGRADE)
        .u64(query_id)
        .cell_ref(new_code.clone())
        .finish()
}

/// Build the StakeVault `StakeCancelUpgrade` body (`queryId`): governance safety
/// valve to abort a pending announcement before it is applied (§8.6).
pub fn build_stake_cancel_upgrade(query_id: u64) -> MessageBody {
    MessageBody::new(OP_STAKE_CANCEL_UPGRADE)
        .u64(query_id)
        .finish()
}

/// Build the StakeVault `StakeClaim` body (`queryId, recipient`) — the C1 pull
/// path that re-delivers a queued/bounced payout (slash split / keeper bounty /
/// owner withdrawal) to the `recipient` it is keyed under. Anyone may trigger it
/// (keeper-friendly), but the vault only ever sends the funds to that recipient,
/// so it cannot redirect a payout.
pub fn build_stake_claim(query_id: u64, recipient: &WalletAddress) -> MessageBody {
    MessageBody::new(OP_STAKE_CLAIM)
        .u64(query_id)
        .addr(recipient)
        .finish()
}

/// The authoritative ecosystem-wide policy read back from the on-chain
/// `GlobalParams` contract (BLOCKCHAIN_ECONOMICS §12). It is the **single source
/// of truth** for paid jobs: the monotonic `version` is what a job binds to, and
/// the values override local config defaults so all parties price/penalize a
/// paid job by the same on-chain policy.
///
/// Only the subset that paid jobs and dispute resolution actually consume is
/// read here (over the single-int `run_get_int` RPC seam) — the full param set
/// is the contract's `get_params` cell (parsed by the `ton-live` harness).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OnchainPolicy {
    /// Monotonic params version/seqno (`get_params_version`) — the value bound
    /// into each job record / escrow so settlement references exact params.
    pub version: u32,
    pub platform_fee_bps: u16,
    /// The authoritative platform-fee RECIPIENT (`get_fee_recipient`) for this
    /// `version` — the admin treasury that EVERY paid job's fee must go to. Read
    /// from chain alongside `platform_fee_bps` so the recipient is enforced by the
    /// same on-chain source of truth, not by local config. Honest parties bind THIS
    /// as the escrow `treasury` and reject any escrow whose treasury differs.
    pub fee_recipient: WalletAddress,
    pub participation_commission_bps: u16,
    pub slash_failed_commitment_bps: u16,
    pub attempt_deadline_ms: u32,
    pub progress_interval_ms: u32,
    pub progress_stall_mult: u8,
}

impl OnchainPolicy {
    /// Overlay this on-chain policy onto the local config layers as the
    /// AUTHORITATIVE layer for paid jobs (on-chain policy wins over local
    /// defaults). Economic fractions land in `[economics]`; the resilience
    /// fairness gating lands in `[scheduler]`. Node-local-only knobs (host
    /// `job_timeout_ms`, retry budgets, liveness) are deliberately untouched.
    pub fn apply_to(&self, econ: &mut EconomicsConfig, sched: &mut SchedulerConfig) {
        let frac = |bps: u16| bps as f64 / 10_000.0;
        econ.fees.platform_fee_pct = frac(self.platform_fee_bps);
        econ.fees.participation_commission_frac = frac(self.participation_commission_bps);
        econ.slashing.slash_failed_commitment_pct = frac(self.slash_failed_commitment_bps);
        sched.attempt_deadline_ms = self.attempt_deadline_ms as u64;
        sched.progress_interval_ms = self.progress_interval_ms as u64;
        sched.progress_stall_multiplier = self.progress_stall_mult.max(1) as u32;
    }
}

/// Read-only client for the on-chain `GlobalParams` contract. Resolves the
/// authoritative ecosystem policy (version + the values paid jobs read) over the
/// existing [`TonRpc`] get-method seam, so on-chain policy can be synced into the
/// config layering (or polled periodically) without any write path.
pub struct GlobalParamsClient<R: TonRpc> {
    rpc: R,
    address: WalletAddress,
}

impl<R: TonRpc> GlobalParamsClient<R> {
    /// Bind to the (stable, hard-pinnable) deployed `GlobalParams` address.
    pub fn new(rpc: R, address: WalletAddress) -> Self {
        Self { rpc, address }
    }

    /// The monotonic params version/seqno (`get_params_version`). This is the
    /// value the coordinator pins into each paid job's record / escrow so all
    /// parties agree which params the job ran under.
    pub fn params_version(&self) -> Result<u32, SettleError> {
        let v = self.rpc.run_get_int(&self.address, "get_params_version")?;
        Ok(v.max(0) as u32)
    }

    /// The monotonic CODE version/seqno (`get_code_version`) — bumped on every
    /// in-place `upgrade_code` (TVM SETCODE) at this STABLE address (§12.1).
    /// 0 = original deployed code. Distinct from [`Self::params_version`]: this
    /// tracks CODE swaps, that one tracks data/param edits. Lets off-chain code
    /// learn which contract code is live without the address ever changing.
    pub fn code_version(&self) -> Result<u32, SettleError> {
        let v = self.rpc.run_get_int(&self.address, "get_code_version")?;
        Ok(v.max(0) as u32)
    }

    /// Read the authoritative ecosystem policy (version + the values paid jobs
    /// consume) via the contract's scalar get-methods.
    pub fn read_policy(&self) -> Result<OnchainPolicy, SettleError> {
        let int = |m: &str| self.rpc.run_get_int(&self.address, m);
        let u16c = |v: i128| v.clamp(0, u16::MAX as i128) as u16;
        let u32c = |v: i128| v.clamp(0, u32::MAX as i128) as u32;
        let u8c = |v: i128| v.clamp(0, u8::MAX as i128) as u8;
        // The authoritative fee RECIPIENT (an address) is read over the dedicated
        // address seam (the single-int seam cannot carry a 256-bit account id). If
        // the transport cannot read it, fail loudly rather than bind a wrong treasury.
        let fee_recipient = self
            .rpc
            .run_get_address(&self.address, "get_fee_recipient")?
            .ok_or_else(|| {
                SettleError::Backend(
                    "GlobalParams.get_fee_recipient unreadable on this transport".into(),
                )
            })?;
        Ok(OnchainPolicy {
            version: u32c(int("get_params_version")?),
            platform_fee_bps: u16c(int("get_platform_fee_bps")?),
            fee_recipient,
            participation_commission_bps: u16c(int("get_participation_commission_bps")?),
            slash_failed_commitment_bps: u16c(int("get_slash_failed_commitment_bps")?),
            attempt_deadline_ms: u32c(int("get_attempt_deadline_ms")?),
            progress_interval_ms: u32c(int("get_progress_interval_ms")?),
            progress_stall_mult: u8c(int("get_progress_stall_mult")?),
        })
    }

    /// Read the on-chain policy and overlay it onto the config layers as the
    /// authoritative layer (returns the policy that was applied). Call this at
    /// startup or on a periodic sync so paid jobs follow on-chain policy.
    pub fn sync_into(
        &self,
        econ: &mut EconomicsConfig,
        sched: &mut SchedulerConfig,
    ) -> Result<OnchainPolicy, SettleError> {
        let policy = self.read_policy()?;
        policy.apply_to(econ, sched);
        Ok(policy)
    }
}

/// Object-safe read seam for the on-chain `GlobalParams` policy, so the live
/// coordinator can hold a `dyn ParamsSource` (it cannot hold the generic
/// [`GlobalParamsClient<R>`]) and sync at startup + on a periodic interval
/// without pulling the TON RPC generic up into the node crate. Implemented by
/// [`GlobalParamsClient`] over any [`TonRpc`]; a node with no live rail simply
/// wires none (free/local jobs never read the chain).
pub trait ParamsSource: Send + Sync {
    /// Read the authoritative ecosystem policy (version + the values paid jobs
    /// consume) from the on-chain `GlobalParams` contract.
    fn read_policy(&self) -> Result<OnchainPolicy, SettleError>;
    /// The monotonic params version/seqno in force (defaults to the version of
    /// [`ParamsSource::read_policy`]).
    fn params_version(&self) -> Result<u32, SettleError> {
        Ok(self.read_policy()?.version)
    }
}

impl<R: TonRpc> ParamsSource for GlobalParamsClient<R> {
    fn read_policy(&self) -> Result<OnchainPolicy, SettleError> {
        GlobalParamsClient::read_policy(self)
    }
    fn params_version(&self) -> Result<u32, SettleError> {
        GlobalParamsClient::params_version(self)
    }
}

// ---------------------------------------------------------------------------
// Deterministic StateInit address derivation (BLOCKCHAIN_ECONOMICS §6.2, §8)
//
// TON contracts have a deterministic address `(workchain, repr_hash(StateInit))`
// where the StateInit carries the contract `code` + initial `data`. So the
// per-node `StakeVault` and per-job `JobEscrow` addresses are known BEFORE
// deploy: the off-chain coordinator builds the same init data cell the deployer
// uses and resolves exactly which contract a node/job maps to (replacing the
// former `blake3(node)` placeholder). The init data layouts below mirror
// `ton/contracts/stake_types.tolk::VaultStorage` and
// `escrow_types.tolk::EscrowStorage` field-for-field.
// ---------------------------------------------------------------------------

/// Per-node `StakeVault` init parameters (the fields that vary per node). The
/// shared `code`/`config` cells are held by [`TonStakeRegistry`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultInit {
    /// Node operator's bound wallet (the vault `owner`).
    pub owner: WalletAddress,
    /// Wallet<->node identity binding hash (§3.2), as stored at deploy.
    pub binding_hash: Hash32,
}

/// Build the per-node `StakeVault` init **data** cell (fresh vault: zero stake),
/// matching `VaultStorage.toCell()` field order: `owner, staked, unbondingAmount,
/// unbondingAt, totalSupply, bindingHash, config(ref), upgrade(ref), pending(dict)`.
///
/// The `upgrade` field is the timelocked code-upgrade state (§8.6), kept in a
/// CHILD cell (a `^ref`) — mirroring `stake_types.tolk::VaultStorage` +
/// `freshVaultUpgrade()`. A fresh vault's upgrade child is all-zero:
/// `codeVersion(32)=0, pendingCodeHash(256)=0, pendingCodeAt(32)=0`. The trailing
/// `pending` field (C1 bounce-safe pull-withdrawal ledger) is an empty
/// `map<address, coins>` serialized as a single empty-dict `0` bit. Both
/// participate in the deterministic StateInit address derivation (cross-checked
/// against the Acton emulator probe, see `vault_data_state_init_matches_onchain`).
pub fn build_vault_data(init: &VaultInit, config: Cell) -> Cell {
    let upgrade = CellBuilder::new()
        .store_uint(0, 32) // codeVersion
        .store_u256(&[0u8; 32]) // pendingCodeHash
        .store_uint(0, 32) // pendingCodeAt
        .build();
    CellBuilder::new()
        .store_address(&init.owner)
        .store_coins(0) // staked
        .store_coins(0) // unbondingAmount
        .store_uint(0, 32) // unbondingAt
        .store_coins(0) // totalSupply
        .store_u256(&init.binding_hash)
        .store_ref(config)
        .store_ref(upgrade)
        .store_dict(ADDRESS_KEY_BITS, &[]) // pending: empty map<address, coins> ⇒ `0` bit
        .build()
}

/// Slash-split parameters, mirroring `stake_types.tolk::SlashConfig` field order:
/// `treasury, redundancyPool, splitChallengerBps(16), splitRedundancyBps(16),
/// splitBurnBps(16), splitTreasuryBps(16)`.
#[derive(Debug, Clone)]
pub struct SlashSplitConfig {
    pub treasury: WalletAddress,
    pub redundancy_pool: WalletAddress,
    pub split_challenger_bps: u16,
    pub split_redundancy_bps: u16,
    pub split_burn_bps: u16,
    pub split_treasury_bps: u16,
}

/// Shared `StakeVault` config parameters, mirroring `stake_types.tolk::VaultConfig`
/// field order: `minStake(coins), unbondingPeriod(32), challengeWindow(32),
/// keeperGrace(32), keeperBountyBps(16), slasher(addr), upgradeAuthority(addr),
/// receiptWalletCode(^), slash(^)`.
#[derive(Debug, Clone)]
pub struct VaultConfigParams {
    pub min_stake: Amount,
    pub unbonding_period: u32,
    pub challenge_window: u32,
    pub keeper_grace: u32,
    pub keeper_bounty_bps: u16,
    pub slasher: WalletAddress,
    pub upgrade_authority: WalletAddress,
    pub receipt_wallet_code: Cell,
    pub slash: SlashSplitConfig,
}

/// Build the `SlashConfig` child cell (`SlashConfig.toCell()`).
pub fn build_slash_config(s: &SlashSplitConfig) -> Cell {
    CellBuilder::new()
        .store_address(&s.treasury)
        .store_address(&s.redundancy_pool)
        .store_uint(s.split_challenger_bps as u128, 16)
        .store_uint(s.split_redundancy_bps as u128, 16)
        .store_uint(s.split_burn_bps as u128, 16)
        .store_uint(s.split_treasury_bps as u128, 16)
        .build()
}

/// Build the shared `VaultConfig` cell (`VaultConfig.toCell()`) — the `config`
/// child every per-node vault references, byte-faithful to `stake_types.tolk`.
///
/// This is the Rust counterpart of the TypeScript `buildStakeVaultDeploy` config
/// section. NOTE: it is NOT yet wired into the live `resolve_onchain_stack` — see
/// the wiring blocker: a deployed-address PARITY check (requires the real
/// `StakeVault` + `StakeReceiptWallet` code BoCs and a fully-recorded deploy
/// vector via the Acton emulator) must pass before the live rail trusts it.
pub fn build_vault_config(c: &VaultConfigParams) -> Cell {
    CellBuilder::new()
        .store_coins(c.min_stake)
        .store_uint(c.unbonding_period as u128, 32)
        .store_uint(c.challenge_window as u128, 32)
        .store_uint(c.keeper_grace as u128, 32)
        .store_uint(c.keeper_bounty_bps as u128, 16)
        .store_address(&c.slasher)
        .store_address(&c.upgrade_authority)
        .store_ref(c.receipt_wallet_code.clone())
        .store_ref(build_slash_config(&c.slash))
        .build()
}

/// Per-job `JobEscrow` init parameters (mirrors `EscrowStorage`, fresh escrow:
/// `settled = false`). `terms` is the `EscrowTerms` child cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EscrowInit {
    pub requester: WalletAddress,
    pub arbiter: WalletAddress,
    pub escrow_amount: Amount,
    pub deadline: u32,
}

/// Build the per-job `EscrowTerms` child cell (mirrors `escrow_types.tolk`:
/// `treasury: address, expectedHash: uint256, candidatesHash: uint256,
/// paramsVersion: uint32, platformFeeBps: uint16, participationCommissionBps: uint16`).
///
/// `platform_fee_bps` is the admin's authoritative platform-fee rate φ (bps) bound
/// at open from `GlobalParams.get_platform_fee_bps` for `params_version`; the
/// escrow then enforces `platformFee == winnerAmount * platform_fee_bps / 10000`
/// paid to `treasury` (= the admin's `feeRecipient`) at settle, so the platform's
/// exact cut is guaranteed and the fee recipient cannot be redirected.
///
/// `expected_hash` is the HTLC lock — settle must later present exactly this
/// agreed quorum result hash. `candidates_hash` is the requester's B1 commitment
/// to the payout-eligible candidate set ([`candidates_commitment`]); settle must
/// present a `candidates` map that hashes to exactly this value, and every payee
/// must be a member, so a compromised arbiter cannot pay an outside address. `0`
/// means "unbound" (the deploy script's default). `params_version` is the on-chain
/// `GlobalParams` version in force when the job opened. All three are bound into
/// the terms (hence into the escrow's deterministic address) so the host,
/// requester and coordinator all agree on the lock, the eligible set, and the
/// params up front. NOTE: `candidatesHash` is inserted BETWEEN `expectedHash` and
/// `paramsVersion`, which CHANGES the escrow's deterministic address vs the
/// pre-B1 layout.
pub fn build_escrow_terms(
    treasury: &WalletAddress,
    expected_hash: &[u8; 32],
    candidates_hash: &[u8; 32],
    params_version: u32,
    platform_fee_bps: u16,
    participation_commission_bps: u16,
) -> Cell {
    CellBuilder::new()
        .store_address(treasury)
        .store_u256(expected_hash)
        .store_u256(candidates_hash)
        .store_uint(params_version as u128, 32)
        // FEE ENFORCEMENT (φ): the admin's platform-fee rate (bps) bound at open so
        // the escrow enforces `platformFee == base*φ` to `treasury` at settle.
        .store_uint(platform_fee_bps as u128, 16)
        // COMMISSION ENFORCEMENT (κ): the admin's participation-commission rate (bps)
        // bound at open so the escrow enforces each participation leg == `base*κ`.
        // Appended last → additive layout (matches escrow_types.tolk::EscrowTerms).
        .store_uint(participation_commission_bps as u128, 16)
        .build()
}

/// Decode a contract **code** cell from a base64 BoC — e.g. the `code_boc64`
/// field of an Acton `ton/build/<Contract>.json` artifact (the compiled
/// `JobEscrow` code). This is how the live `JobEscrow` code is wired into
/// [`TonSettlement::with_escrow_code`] for deterministic address derivation +
/// funded deploy, without an Acton-runtime `build("JobEscrow")` call.
pub fn escrow_code_from_boc_base64(code_boc64: &str) -> Result<Cell, SettleError> {
    let bytes = crate::wallet::base64_decode(code_boc64.trim())
        .ok_or_else(|| SettleError::Backend("escrow code: invalid base64 BoC".into()))?;
    Cell::from_boc(&bytes)
        .ok_or_else(|| SettleError::Backend("escrow code: BoC failed to parse".into()))
}

/// Build the per-job `JobEscrow` init **data** cell, matching `EscrowStorage`:
/// `requester, arbiter, escrowAmount, deadline, settled, terms(ref), pending(dict)`.
///
/// The trailing `pending` field (B2 bounce-safe pull-withdrawal ledger) is an
/// empty `map<address, coins>` serialized as a single empty-dict `0` bit; it is
/// part of the init data, so it participates in the deterministic StateInit
/// address derivation (cross-checked against `ton/scripts/_probe_v2.tolk`).
pub fn build_escrow_data(init: &EscrowInit, terms: Cell) -> Cell {
    CellBuilder::new()
        .store_address(&init.requester)
        .store_address(&init.arbiter)
        .store_coins(init.escrow_amount)
        .store_uint(init.deadline as u128, 32)
        .store_uint(0, 1) // settled = false
        .store_ref(terms)
        .store_dict(ADDRESS_KEY_BITS, &[]) // pending: empty map<address, coins> ⇒ `0` bit
        .build()
}

/// Abstract TON transport. The live HTTP/toncenter client is gated behind
/// `ton-live`; tests use a recording fake.
pub trait TonRpc: Send + Sync {
    /// Send an internal message carrying `body` to `to` with `amount` nanoton.
    /// Returns a tx hash / id on success.
    fn send_internal(
        &self,
        to: &WalletAddress,
        amount: Amount,
        body: &MessageBody,
    ) -> Result<String, SettleError>;
    /// Run a get-method returning a single integer (e.g. staked amount).
    fn run_get_int(&self, addr: &WalletAddress, method: &str) -> Result<i128, SettleError>;
    /// Run a get-method returning a single ADDRESS (a `MsgAddress` slice), e.g.
    /// `GlobalParams.get_fee_recipient`. The single-int [`TonRpc::run_get_int`] seam
    /// cannot carry a 256-bit account id, so this is a distinct capability. `Ok(None)`
    /// (the default) means "this transport cannot read addresses" — callers that
    /// require the value (the fee-recipient sync) then surface a clear error rather
    /// than silently binding a wrong treasury. The live toncenter client overrides it
    /// to parse the address slice from the get-method result stack.
    fn run_get_address(
        &self,
        _addr: &WalletAddress,
        _method: &str,
    ) -> Result<Option<WalletAddress>, SettleError> {
        Ok(None)
    }
    /// Deploy a contract: send `amount` nanoton + `state_init` to `to` carrying
    /// `body` (used to fund-open a per-job escrow). Default: unsupported.
    fn deploy(
        &self,
        _to: &WalletAddress,
        _amount: Amount,
        _state_init: &Cell,
        _body: &MessageBody,
    ) -> Result<String, SettleError> {
        Err(SettleError::Backend(
            "deploy is not supported by this transport".into(),
        ))
    }

    /// Read `RecordAnchor.get_anchor_state` → `(currentEpoch, lastRoot)`. This
    /// needs the FULL get-method stack (the single-int [`TonRpc::run_get_int`]
    /// seam only sees `currentEpoch`, the first item, not the 256-bit `lastRoot`),
    /// so it is a distinct capability. `Ok(None)` (the default) means "this
    /// transport cannot read the live anchor state" — [`TonRecordAnchor::submit_root`]
    /// then falls back to its locally tracked chain. The live toncenter client
    /// overrides it to parse the real on-chain state for keeper idempotency (so a
    /// retry / a second keeper that already advanced the chain cannot desync).
    fn run_get_anchor_state(
        &self,
        _addr: &WalletAddress,
    ) -> Result<Option<(u32, Hash32)>, SettleError> {
        Ok(None)
    }

    /// The logical time (`lt`) of the most recent transaction recorded on
    /// `account`, or `0` if it has none. Captured *before* a send so that
    /// [`TonRpc::await_confirmation`] can tell "the transaction we just caused"
    /// apart from older ones. Offline/null default: `0` (no chain to read).
    fn last_tx_lt(&self, _account: &WalletAddress) -> Result<u64, SettleError> {
        Ok(0)
    }

    /// Block (polling, up to `deadline_secs`) until a transaction newer than
    /// `after_lt` lands on `account` and assert it executed **successfully**
    /// (compute + action phases, not aborted).
    ///
    /// This is what upgrades a fire-and-forget `send_internal`/`deploy` — which
    /// only confirms the message reached the *mempool* — into a real guarantee
    /// that the destination contract actually *processed* it. Returns:
    ///  * `Ok(())` — a newer transaction landed and succeeded;
    ///  * `Err(SettleError::TxFailed { exit_code })` — it ran but failed/aborted;
    ///  * `Err(SettleError::TxUnconfirmed)` — nothing landed in time (dropped /
    ///    in-flight).
    ///
    /// Offline/null default: `Ok(())` — the mock/no-op rails never touch a chain,
    /// so there is nothing to confirm. The live toncenter client overrides it.
    fn await_confirmation(
        &self,
        _account: &WalletAddress,
        _after_lt: u64,
        _deadline_secs: u64,
    ) -> Result<(), SettleError> {
        Ok(())
    }
}

/// How long to wait for an on-chain transaction to be confirmed before giving up
/// (`SettleError::TxUnconfirmed`). Matched to the external-message `valid_until`
/// window (90s): past it the signed message can no longer be included, so it will
/// never confirm.
pub const CONFIRM_TIMEOUT_SECS: u64 = 90;

/// Default transport that performs NO network I/O. Used when `ton-live` is off.
#[derive(Default)]
pub struct NullTonRpc;

impl TonRpc for NullTonRpc {
    fn send_internal(
        &self,
        _to: &WalletAddress,
        _amount: Amount,
        _body: &MessageBody,
    ) -> Result<String, SettleError> {
        Err(SettleError::Backend(
            "live TON RPC disabled (build with feature `ton-live` and configure an endpoint)"
                .into(),
        ))
    }
    fn run_get_int(&self, _addr: &WalletAddress, _method: &str) -> Result<i128, SettleError> {
        Err(SettleError::Backend("live TON RPC disabled".into()))
    }
}

/// TON-backed settlement (per-job `JobEscrow`). The per-job escrow address is
/// derived deterministically from its `StateInit` (escrow code + per-job init
/// data) so the coordinator knows the address before the funded deploy lands.
pub struct TonSettlement<R: TonRpc> {
    rpc: R,
    /// Compiled `JobEscrow` code cell (shared across jobs). When `None`, escrow
    /// addresses cannot be derived and `open_escrow` falls back to the
    /// deploy-path error (e.g. code not yet wired from `[economics.contracts]`).
    escrow_code: Option<Cell>,
    /// Shared `EscrowTerms` child cell (treasury + expected-hash layout).
    terms: Cell,
    /// Quorum oracle / coordinator authorized to settle.
    arbiter: WalletAddress,
    /// Requester wallet that funds + receives refunds (needed to deploy escrow).
    requester: Option<WalletAddress>,
    /// Platform-fee recipient written into a freshly-built per-job `EscrowTerms`
    /// (the `treasury` field). When `None`, [`Settlement::open_escrow_with_terms`]
    /// falls back to the shared `terms` cell / arbiter.
    treasury: Option<WalletAddress>,
    /// Refund-on-timeout window (secs) used to set a fresh escrow's deadline.
    escrow_window_secs: u32,
    /// Extra value (nanoton) attached to the funded deploy ON TOP OF the locked
    /// bid `B`, so the per-job escrow physically holds enough to cover its own
    /// compute + action (forward) fees when it pays out the split at settle time.
    /// The `JobEscrow` pays `winner + fee + participants + refund == escrowAmount`
    /// with `PAY_FEES_SEPARATELY`, so without this headroom a real settle's action
    /// phase fails for lack of balance (the Acton deploy script funds the same
    /// way: `escrowAmount + ton("0.15")`). The stored `escrowAmount` / handle
    /// `max_bid` stay exactly `B`; the buffer is not part of the locked bid and is
    /// not accounted in the payout. `0` (default) preserves the exact-`B` deploy
    /// (used by the offline ABI tests); the live rail sets a small buffer.
    deploy_gas_buffer: Amount,
    /// Value (nanoton) attached to the `EscrowSettle` / `EscrowRefund` message so
    /// the escrow's COMPUTE phase has gas. On TON the compute-phase gas is funded
    /// by the incoming message value, so a 0-value internal message aborts BEFORE
    /// compute and never runs the settle logic (the escrow stays unsettled). The
    /// bounded `B` split is still paid from the escrow's own deploy-funded balance;
    /// this only funds the settle compute + a little headroom. `0` (default)
    /// preserves the offline tests' zero-value send; the live rail sets a small
    /// amount.
    settle_gas: Amount,
    /// Monotonic `queryId` source: each `EscrowTopUp` / `EscrowSettle` /
    /// `EscrowRefund` the bridge broadcasts is stamped with a fresh value (see
    /// [`QueryIdGen`]) so the messages are distinguishable on-chain instead of all
    /// carrying `0`.
    qid: QueryIdGen,
    /// B1: the requester's pre-committed payout-eligible candidate set. When set,
    /// it is bound into a freshly built `EscrowTerms.candidatesHash` at open
    /// (`open_escrow_with_terms`) AND presented as the `candidates` map at settle —
    /// they MUST be the same set for the contract's commitment check to pass. When
    /// EMPTY (the default), open binds the empty-set commitment and settle derives
    /// the candidate set from the outcome's payees (winner ∪ participants) so a
    /// settle still presents a well-formed, membership-consistent map. See the
    /// module note on the coordinator wiring.
    candidates: Vec<WalletAddress>,
    /// FEE ENFORCEMENT (φ): the admin's authoritative platform-fee rate (bps) bound
    /// into a freshly built `EscrowTerms.platformFeeBps` at open. The on-chain
    /// escrow then REQUIRES `platformFee == winnerAmount * platform_fee_bps / 10000`
    /// to `treasury` (the admin `feeRecipient`) at settle. This must equal the φ the
    /// coordinator computes the split with (both source the config / synced policy)
    /// and the φ the deployed `GlobalParams` holds for the pinned params version. `0`
    /// (default) ⇒ no fee enforced (legacy/offline ABI tests with explicit fees).
    platform_fee_bps: u16,
    /// COMMISSION ENFORCEMENT (κ): the admin's authoritative participation-commission
    /// rate (bps) bound into a freshly built `EscrowTerms.participationCommissionBps`
    /// at open. When > 0 the on-chain escrow REQUIRES each participation leg to equal
    /// EXACTLY `base * κ / 10000` — so a compromised arbiter cannot shave the agreeing
    /// verifiers' promised commission. Must equal the κ the coordinator computes its
    /// split with (synced `GlobalParams.get_participation_commission_bps`). `0`
    /// (default) ⇒ no commission enforced.
    participation_commission_bps: u16,
}

impl<R: TonRpc> TonSettlement<R> {
    /// Construct without escrow-code wiring (address derivation disabled).
    pub fn new(rpc: R) -> Self {
        Self {
            rpc,
            escrow_code: None,
            terms: Cell::default(),
            arbiter: WalletAddress::new(BASECHAIN, [0u8; 32]),
            requester: None,
            treasury: None,
            escrow_window_secs: 3600,
            deploy_gas_buffer: 0,
            settle_gas: 0,
            qid: QueryIdGen::new(),
            candidates: Vec::new(),
            platform_fee_bps: 0,
            participation_commission_bps: 0,
        }
    }

    /// Construct with the deployed `JobEscrow` code so per-job escrow addresses
    /// can be resolved via deterministic StateInit addressing.
    pub fn with_escrow_code(
        rpc: R,
        escrow_code: Cell,
        terms: Cell,
        arbiter: WalletAddress,
    ) -> Self {
        Self {
            rpc,
            escrow_code: Some(escrow_code),
            terms,
            arbiter,
            requester: None,
            treasury: None,
            escrow_window_secs: 3600,
            deploy_gas_buffer: 0,
            settle_gas: 0,
            qid: QueryIdGen::new(),
            candidates: Vec::new(),
            platform_fee_bps: 0,
            participation_commission_bps: 0,
        }
    }

    /// FEE ENFORCEMENT (φ): bind the admin's platform-fee rate (bps) into the
    /// per-job `EscrowTerms` at open. The on-chain escrow then requires the settle's
    /// `platformFee` to be EXACTLY `winnerAmount * platform_fee_bps / 10000` paid to
    /// the bound `treasury` (the admin fee recipient). Set this to the same φ the
    /// coordinator computes its split with and the deployed `GlobalParams` holds.
    pub fn with_platform_fee_bps(mut self, platform_fee_bps: u16) -> Self {
        self.platform_fee_bps = platform_fee_bps;
        self
    }

    /// COMMISSION ENFORCEMENT (κ): bind the admin's participation-commission rate
    /// (bps) into the per-job `EscrowTerms` at open. When > 0 the on-chain escrow
    /// requires each participation leg to be EXACTLY `base * κ / 10000`. Set this to
    /// the same κ the coordinator computes its split with (synced `GlobalParams`).
    pub fn with_participation_commission_bps(mut self, participation_commission_bps: u16) -> Self {
        self.participation_commission_bps = participation_commission_bps;
        self
    }

    /// Set the requester wallet (funds + refund recipient) so `open_escrow` can
    /// deploy the funded per-job escrow.
    pub fn with_requester(mut self, requester: WalletAddress) -> Self {
        self.requester = Some(requester);
        self
    }

    /// Set the platform-fee `treasury` written into per-job `EscrowTerms` built by
    /// [`Settlement::open_escrow_with_terms`]. Without it the shared `terms` cell
    /// (or the arbiter) is used as the treasury.
    pub fn with_treasury(mut self, treasury: WalletAddress) -> Self {
        self.treasury = Some(treasury);
        self
    }

    /// Set the refund-on-timeout window (secs) for newly opened escrows.
    pub fn with_escrow_window(mut self, secs: u32) -> Self {
        self.escrow_window_secs = secs;
        self
    }

    /// Set the deploy gas buffer (nanoton) attached to the funded deploy on top of
    /// the locked bid `B`, so the per-job escrow can pay its own settle-time
    /// compute + forward fees (see [`TonSettlement::deploy_gas_buffer`]). The
    /// stored `escrowAmount` / handle `max_bid` remain exactly `B`.
    pub fn with_deploy_gas_buffer(mut self, nanoton: Amount) -> Self {
        self.deploy_gas_buffer = nanoton;
        self
    }

    /// Set the gas (nanoton) attached to the `EscrowSettle` / `EscrowRefund`
    /// message so the escrow's compute phase can run (see
    /// [`TonSettlement::settle_gas`]). Required on a live rail; a 0-value settle
    /// message aborts before compute and never settles.
    pub fn with_settle_gas(mut self, nanoton: Amount) -> Self {
        self.settle_gas = nanoton;
        self
    }

    /// Bind the requester's B1 payout-eligible candidate set (the N dispatched
    /// workers' payout wallets). This is committed into a freshly built
    /// `EscrowTerms.candidatesHash` at open AND re-presented as the `candidates`
    /// map at settle, so the on-chain commitment check passes and every payee is a
    /// member. The same set MUST be used at open and settle; bind it via this
    /// builder BEFORE opening the escrow. When unset, open binds the empty-set
    /// commitment and settle derives candidates from the outcome's payees (see
    /// [`TonSettlement::candidates`]).
    pub fn with_candidates(mut self, candidates: Vec<WalletAddress>) -> Self {
        self.candidates = candidates;
        self
    }

    /// Deterministic per-job `JobEscrow` address from its StateInit, or `None`
    /// when the escrow code has not been wired in.
    pub fn escrow_address(
        &self,
        requester: &WalletAddress,
        max_bid: Amount,
        deadline: u32,
    ) -> Option<WalletAddress> {
        let code = self.escrow_code.as_ref()?;
        let init = EscrowInit {
            requester: *requester,
            arbiter: self.arbiter,
            escrow_amount: max_bid,
            deadline,
        };
        let data = build_escrow_data(&init, self.terms.clone());
        Some(WalletAddress::from_state_init(BASECHAIN, code, &data))
    }
}

impl<R: TonRpc> Settlement for TonSettlement<R> {
    fn open_escrow(&self, job: &JobId, max_bid: Amount) -> Result<EscrowHandle, SettleError> {
        // Deploy the per-job `JobEscrow` funded with the locked bid `B`: build its
        // deterministic StateInit (escrow code + per-job init data), then send a
        // funded deploy carrying an `EscrowTopUp` body. The deploy itself is
        // performed by the transport (`deploy`), which the live toncenter client
        // implements and the default (`NullTonRpc`) reports as unsupported.
        let code = self.escrow_code.as_ref().ok_or_else(|| {
            SettleError::Backend("open_escrow requires escrow code wiring".into())
        })?;
        let requester = self.requester.ok_or_else(|| {
            SettleError::Backend("open_escrow requires a requester wallet".into())
        })?;
        let deadline = now_secs_u32().saturating_add(self.escrow_window_secs);
        let init = EscrowInit {
            requester,
            arbiter: self.arbiter,
            escrow_amount: max_bid,
            deadline,
        };
        let data = build_escrow_data(&init, self.terms.clone());
        let address = WalletAddress::from_state_init(BASECHAIN, code, &data);
        let state_init = state_init_cell(code.clone(), data);
        let body = build_escrow_topup(self.qid.next());
        // Fund the deploy with B + the gas buffer so the escrow can pay its own
        // settle-time fees; `escrowAmount` (stored) and the handle stay exactly B.
        let deploy_value = max_bid.saturating_add(self.deploy_gas_buffer);
        // Confirm the funded deploy actually lands + succeeds before returning a
        // handle the caller will settle against (a fresh escrow has no prior txs).
        let before = self.rpc.last_tx_lt(&address)?;
        self.rpc
            .deploy(&address, deploy_value, &state_init, &body)?;
        self.rpc
            .await_confirmation(&address, before, CONFIRM_TIMEOUT_SECS)?;
        Ok(EscrowHandle {
            job: job.clone(),
            address,
            max_bid,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn open_escrow_with_terms(
        &self,
        job: &JobId,
        max_bid: Amount,
        expected_hash: &Hash32,
        params_version: u32,
        candidates: &[WalletAddress],
        fee_recipient: Option<WalletAddress>,
        platform_fee_bps: Option<u16>,
        participation_commission_bps: Option<u16>,
    ) -> Result<EscrowHandle, SettleError> {
        // Build a FRESH per-job `EscrowTerms` binding the HTLC lock (the agreed
        // quorum result hash) + the B1 candidate-set commitment + the on-chain
        // params version, deploy the per-job escrow funded with `B`, and return the
        // handle. The escrow address is a pure function of (code, requester,
        // arbiter, B, deadline, terms), so it commits to `expected_hash`, the
        // candidate set, and `params_version` up front.
        let code = self.escrow_code.as_ref().ok_or_else(|| {
            SettleError::Backend("open_escrow requires escrow code wiring".into())
        })?;
        let requester = self.requester.ok_or_else(|| {
            SettleError::Backend("open_escrow requires a requester wallet".into())
        })?;
        // FEE ENFORCEMENT (φ): the rate bound into the terms (and enforced at
        // settle). Prefer the caller's chain-synced φ (the value the coordinator
        // computes the split with for `params_version`) over the rail's wired φ,
        // so an admin fee change can never desync the bound terms from the split
        // (which would make settle revert FEE_MISMATCH). Fall back to the wired
        // value when the caller passes none (no params source).
        let fee_bps = platform_fee_bps.unwrap_or(self.platform_fee_bps);
        // COMMISSION ENFORCEMENT (κ): same precedence as φ — prefer the caller's
        // chain-synced κ (the value the coordinator computes each verifier
        // commission with) over the wired κ, so the bound terms match the split.
        let comm_bps = participation_commission_bps.unwrap_or(self.participation_commission_bps);
        // FEE-RECIPIENT enforcement: the platform-fee `treasury` is sourced from the
        // AUTHORITATIVE on-chain `GlobalParams.fee_recipient` (`fee_recipient`),
        // NOT from local config. Precedence: chain value wins; a locally-configured
        // `self.treasury` is only an optional cross-check that MUST match the chain
        // value (else we refuse to open — the admin treasury can't be silently
        // replaced).
        let treasury = match fee_recipient {
            Some(chain) => {
                if let Some(local) = self.treasury {
                    if local != chain {
                        tracing::warn!(
                            job = %job.0,
                            local_treasury = %local.to_raw_string(),
                            chain_fee_recipient = %chain.to_raw_string(),
                            params_version,
                            "refusing to open escrow: local treasury disagrees with the \
                             authoritative on-chain GlobalParams.fee_recipient"
                        );
                        return Err(SettleError::TreasuryMismatch {
                            bound: local.to_raw_string(),
                            expected: chain.to_raw_string(),
                            params_version,
                        });
                    }
                }
                chain
            }
            // No authoritative chain recipient (no params source wired). Use a
            // locally-configured treasury if present. Otherwise (M3) the OLD code
            // fell back to `self.arbiter` — paying the platform fee to the
            // COORDINATOR's OWN wallet. Only tolerate the absence when NO fee is
            // actually charged (φ = 0, so the treasury is never paid); when a fee
            // WOULD be collected, refuse to open rather than silently misdirect it.
            None => match self.treasury {
                Some(t) => t,
                None if fee_bps == 0 => self.arbiter,
                None => {
                    tracing::warn!(
                        job = %job.0,
                        fee_bps,
                        "refusing to open escrow: non-zero platform fee but no fee recipient \
                         known (no GlobalParams source wired AND no local treasury)"
                    );
                    return Err(SettleError::Backend(
                        "cannot open escrow: platform fee is non-zero but no fee recipient is \
                         known (no GlobalParams source wired AND no local treasury configured). \
                         Wire a params source or configure the treasury so the fee is not \
                         misdirected to the arbiter/coordinator wallet."
                            .into(),
                    ));
                }
            },
        };
        // B1: bind the candidate-set commitment over the per-job payout set passed
        // by the coordinator (winner ∪ agreeing non-winners). `settle` MUST later
        // present a `candidates` map hashing to exactly this. Fall back to the
        // builder-bound set when the caller passes none (empty ⇒ empty-dict hash).
        let cands: &[WalletAddress] = if candidates.is_empty() {
            &self.candidates
        } else {
            candidates
        };
        let candidates_hash = candidates_commitment(cands);
        // Bind the admin fee recipient (`treasury`) + rate (`fee_bps`) + commission
        // rate (`comm_bps`) so the escrow enforces the exact platform cut AND each
        // verifier's exact κ·base commission at settle.
        let terms = build_escrow_terms(
            &treasury,
            expected_hash,
            &candidates_hash,
            params_version,
            fee_bps,
            comm_bps,
        );
        let deadline = now_secs_u32().saturating_add(self.escrow_window_secs);
        let init = EscrowInit {
            requester,
            arbiter: self.arbiter,
            escrow_amount: max_bid,
            deadline,
        };
        let data = build_escrow_data(&init, terms);
        let address = WalletAddress::from_state_init(BASECHAIN, code, &data);
        let state_init = state_init_cell(code.clone(), data);
        let body = build_escrow_topup(self.qid.next());
        // Fund the deploy with B + the gas buffer (see `open_escrow`); the locked
        // `escrowAmount` and the returned handle remain exactly B.
        let deploy_value = max_bid.saturating_add(self.deploy_gas_buffer);
        // Confirm the funded deploy actually lands + succeeds on-chain BEFORE
        // returning the handle, so the coordinator never settles against an
        // escrow that was never funded/deployed (no fire-and-forget).
        tracing::info!(
            job = %job.0,
            escrow = %address.to_raw_string(),
            locked_b = max_bid,
            deploy_value,
            fee_bps,
            comm_bps,
            params_version,
            treasury = %treasury.to_raw_string(),
            "opening per-job escrow: funded deploy + confirmation"
        );
        let before = self.rpc.last_tx_lt(&address)?;
        self.rpc
            .deploy(&address, deploy_value, &state_init, &body)?;
        if let Err(e) = self
            .rpc
            .await_confirmation(&address, before, CONFIRM_TIMEOUT_SECS)
        {
            tracing::warn!(
                job = %job.0,
                escrow = %address.to_raw_string(),
                "escrow deploy not confirmed on-chain: {e}"
            );
            return Err(e);
        }
        tracing::info!(job = %job.0, escrow = %address.to_raw_string(), "escrow deployed + confirmed");
        Ok(EscrowHandle {
            job: job.clone(),
            address,
            max_bid,
        })
    }

    fn settle(&self, h: &EscrowHandle, outcome: &SettlementOutcome) -> Result<(), SettleError> {
        if outcome.total() > h.max_bid {
            tracing::warn!(
                escrow = %h.address.to_raw_string(),
                payout = outcome.total(),
                escrow_b = h.max_bid,
                "refusing to settle: computed payout exceeds the locked escrow (coverage \
                 preflight) — would revert PAYOUT_EXCEEDS_ESCROW on-chain"
            );
            return Err(SettleError::PayoutExceedsEscrow {
                payout: outcome.total(),
                escrow: h.max_bid,
            });
        }
        // B1: present the requester-committed candidate set. Use the bound set when
        // configured (it must match what `open_escrow_with_terms` committed); else
        // derive it from the outcome's payees (winner ∪ participants) so the
        // presented map is non-empty, well-formed, and contains every payee.
        let candidates: Vec<WalletAddress> = if self.candidates.is_empty() {
            let mut c = vec![outcome.winner.to];
            for p in &outcome.participants {
                if !c.contains(&p.to) {
                    c.push(p.to);
                }
            }
            c
        } else {
            self.candidates.clone()
        };
        let body = build_escrow_settle(
            self.qid.next(),
            &outcome.result_hash,
            &outcome.winner.to,
            outcome.base,
            outcome.winner.amount,
            outcome.platform_fee,
            &outcome.participants,
            &candidates,
        );
        // Attach `settle_gas` so the escrow's compute phase can run (a 0-value
        // internal message aborts before compute). The bounded `B` split is paid
        // from the escrow's own balance, not from this gas.
        tracing::info!(
            escrow = %h.address.to_raw_string(),
            winner = %outcome.winner.to.to_raw_string(),
            winner_amount = outcome.winner.amount,
            base = outcome.base,
            platform_fee = outcome.platform_fee,
            participants = outcome.participants.len(),
            "settling escrow on the agreed quorum hash"
        );
        let before = self.rpc.last_tx_lt(&h.address)?;
        self.rpc.send_internal(&h.address, self.settle_gas, &body)?;
        // Confirm the escrow actually released — settle did not throw on the
        // candidate-commitment / result-hash / fee / commission checks and was not
        // aborted — before reporting the job paid.
        match self
            .rpc
            .await_confirmation(&h.address, before, CONFIRM_TIMEOUT_SECS)
        {
            Ok(()) => {
                tracing::info!(escrow = %h.address.to_raw_string(), "escrow settled + confirmed");
                Ok(())
            }
            Err(e) => {
                tracing::warn!(
                    escrow = %h.address.to_raw_string(),
                    "settle not confirmed (on-chain abort or unverifiable outcome): {e}"
                );
                Err(e)
            }
        }
    }

    fn refund(&self, h: &EscrowHandle) -> Result<(), SettleError> {
        tracing::info!(escrow = %h.address.to_raw_string(), "refunding escrow to requester (timeout)");
        let body = build_escrow_refund(self.qid.next());
        let before = self.rpc.last_tx_lt(&h.address)?;
        self.rpc.send_internal(&h.address, self.settle_gas, &body)?;
        match self
            .rpc
            .await_confirmation(&h.address, before, CONFIRM_TIMEOUT_SECS)
        {
            Ok(()) => {
                tracing::info!(escrow = %h.address.to_raw_string(), "escrow refund confirmed");
                Ok(())
            }
            Err(e) => {
                tracing::warn!(escrow = %h.address.to_raw_string(), "escrow refund not confirmed: {e}");
                Err(e)
            }
        }
    }

    fn is_onchain(&self) -> bool {
        true
    }
}

/// TON-backed stake registry (per-node `StakeVault`). Reads use get-methods; the
/// per-node vault address is derived deterministically from its `StateInit`
/// (shared vault code + shared config + the node's per-node init data), so a node
/// resolves to exactly the vault that was (or will be) deployed for it.
pub struct TonStakeRegistry<R: TonRpc> {
    rpc: R,
    min_public: Amount,
    stake_cap: Amount,
    /// Compiled `StakeVault` code cell (shared across nodes).
    vault_code: Cell,
    /// Shared `VaultConfig` child cell referenced by every vault's storage.
    vault_config: Cell,
    /// Node -> per-node vault init params (owner wallet + binding hash). Built
    /// from the node<->wallet binding records; a node with no entry has no vault.
    inits: HashMap<NodeId, VaultInit>,
    /// Monotonic `queryId` source for the `StakeSlash` / `StakeRequestUnbond`
    /// messages this registry broadcasts (see [`QueryIdGen`]).
    qid: QueryIdGen,
}

impl<R: TonRpc> TonStakeRegistry<R> {
    pub fn new(
        rpc: R,
        min_public: Amount,
        stake_cap: Amount,
        vault_code: Cell,
        vault_config: Cell,
        inits: HashMap<NodeId, VaultInit>,
    ) -> Self {
        Self {
            rpc,
            min_public,
            stake_cap,
            vault_code,
            vault_config,
            inits,
            qid: QueryIdGen::new(),
        }
    }

    /// Deterministic per-node `StakeVault` address from its StateInit, or `None`
    /// if the node has no registered binding (hence no vault).
    pub fn vault_of(&self, node: &NodeId) -> Option<WalletAddress> {
        let init = self.inits.get(node)?;
        let data = build_vault_data(init, self.vault_config.clone());
        Some(WalletAddress::from_state_init(
            BASECHAIN,
            &self.vault_code,
            &data,
        ))
    }
}

impl<R: TonRpc> StakeRegistry for TonStakeRegistry<R> {
    fn stake_of(&self, node: &NodeId) -> Amount {
        let Some(vault) = self.vault_of(node) else {
            return 0;
        };
        match self.rpc.run_get_int(&vault, "get_vault_state") {
            Ok(v) if v >= 0 => v as Amount,
            _ => 0,
        }
    }
    fn is_eligible(&self, node: &NodeId, _class: DataClassCfg) -> bool {
        self.stake_of(node) >= self.min_public
    }
    fn stake_factor(&self, node: &NodeId) -> f64 {
        crate::stake_factor(self.stake_of(node), self.min_public, self.stake_cap)
    }
    fn slash(&self, node: &NodeId, reason: SlashReason, amount: Amount) -> Result<(), SlashError> {
        let vault = self
            .vault_of(node)
            .ok_or_else(|| SlashError::UnknownNode(node.0.clone()))?;
        let body = build_stake_slash(self.qid.next(), amount, reason, &vault);
        let before = self
            .rpc
            .last_tx_lt(&vault)
            .map_err(|e| SlashError::Backend(e.to_string()))?;
        self.rpc
            .send_internal(&vault, 0, &body)
            .map_err(|e| SlashError::Backend(e.to_string()))?;
        // Confirm the vault processed the slash before reporting success.
        self.rpc
            .await_confirmation(&vault, before, CONFIRM_TIMEOUT_SECS)
            .map_err(|e| SlashError::Backend(e.to_string()))
    }
    fn request_unbond(&self, node: &NodeId, amount: Amount) -> Result<(), SlashError> {
        let vault = self
            .vault_of(node)
            .ok_or_else(|| SlashError::UnknownNode(node.0.clone()))?;
        let body = build_stake_unbond(self.qid.next(), amount);
        let before = self
            .rpc
            .last_tx_lt(&vault)
            .map_err(|e| SlashError::Backend(e.to_string()))?;
        self.rpc
            .send_internal(&vault, 0, &body)
            .map_err(|e| SlashError::Backend(e.to_string()))?;
        // Confirm the vault processed the unbond request before reporting success.
        self.rpc
            .await_confirmation(&vault, before, CONFIRM_TIMEOUT_SECS)
            .map_err(|e| SlashError::Backend(e.to_string()))
    }
}

/// TON-backed record anchor (BLOCKCHAIN_ECONOMICS §7): keeps the off-chain epoch
/// Merkle tree (so inclusion proofs are served locally) AND broadcasts the epoch
/// **root** on-chain to the `RecordAnchor` contract via `AnchorSubmitRoot`. Roots
/// chain through `prev_root`, mirroring the on-chain verifier.
pub struct TonRecordAnchor<R: TonRpc> {
    rpc: R,
    /// Deployed `RecordAnchor` contract address.
    anchor: WalletAddress,
    /// Off-chain epoch tree (job -> leaf), and the last submitted root.
    inner: std::sync::Mutex<(Vec<(JobId, Hash32)>, Hash32)>,
    /// Monotonic `queryId` source for the `AnchorSubmitRoot` messages this anchor
    /// broadcasts (see [`QueryIdGen`]).
    qid: QueryIdGen,
}

impl<R: TonRpc> TonRecordAnchor<R> {
    pub fn new(rpc: R, anchor: WalletAddress) -> Self {
        Self {
            rpc,
            anchor,
            inner: std::sync::Mutex::new((Vec::new(), [0u8; 32])),
            qid: QueryIdGen::new(),
        }
    }

    /// Broadcast the current epoch root to the on-chain `RecordAnchor` (keeper
    /// submit). `stake_weight` is the aggregate staked weight backing this root.
    /// Advances the chained `prev_root` on success.
    ///
    /// HARDENING (mirrors the contract's `AnchorSubmitRoot` checks so a bad submit
    /// is refused BEFORE paying gas, and a retry / a second keeper cannot desync):
    ///   * the root is computed with [`crate::merkle::try_merkle_root`] and an
    ///     EMPTY epoch (no records) is REFUSED — there is nothing to anchor, and a
    ///     zero/empty root would collide with the genesis `lastRoot == 0` chain
    ///     check (A6);
    ///   * an all-zero root is REFUSED for the same reason;
    ///   * the live on-chain `(currentEpoch, lastRoot)` is read via
    ///     [`TonRpc::run_get_anchor_state`] and used as the source of truth: the
    ///     submission must be exactly `currentEpoch + 1` (else another keeper
    ///     already advanced the chain → IDEMPOTENT refusal, no desync), and the
    ///     body's `prevRoot` is the on-chain `lastRoot`, NOT a purely-local guess.
    ///     When the transport cannot read live state (`Ok(None)`, e.g. the offline
    ///     default), it falls back to the locally tracked `prev` + a local
    ///     monotonicity guard.
    pub fn submit_root(&self, epoch: u32, stake_weight: Amount) -> Result<String, SettleError> {
        // Compute the root from the local epoch tree; refuse an empty epoch.
        let (root, local_prev) = {
            let g = self.inner.lock().unwrap();
            let leaves: Vec<Hash32> = g.0.iter().map(|(_, h)| *h).collect();
            let root = crate::merkle::try_merkle_root(&leaves).ok_or_else(|| {
                SettleError::Backend("refusing to anchor an empty epoch (no records)".into())
            })?;
            (root, g.1)
        };
        // A6: never anchor an all-zero root (it would pass the genesis prevRoot==0
        // chain check and anchor a meaningless epoch).
        if root == [0u8; 32] {
            return Err(SettleError::Backend(
                "refusing to anchor a zero root".into(),
            ));
        }
        // Read live on-chain chain state for idempotency; fall back to local when
        // the transport can't (offline default).
        let prev = match self.rpc.run_get_anchor_state(&self.anchor)? {
            Some((onchain_epoch, onchain_last)) => {
                // The contract requires epoch == currentEpoch + 1; enforcing it here
                // means a retry (or a second keeper that already advanced) is refused
                // without spending gas, and we chain onto the TRUE on-chain root.
                if epoch != onchain_epoch.saturating_add(1) {
                    return Err(SettleError::Backend(format!(
                        "anchor epoch desync: submitting epoch {epoch} but on-chain currentEpoch is {onchain_epoch} (expected {})",
                        onchain_epoch.saturating_add(1)
                    )));
                }
                onchain_last
            }
            None => local_prev,
        };
        let body = build_anchor_submit(self.qid.next(), epoch, &root, &prev, stake_weight);
        let before = self.rpc.last_tx_lt(&self.anchor)?;
        let res = self.rpc.send_internal(&self.anchor, 0, &body)?;
        // Confirm the anchor accepted the root (keeper auth, stake-weight,
        // prev-root/epoch checks passed, not aborted) BEFORE advancing the locally
        // tracked prev-root — an unconfirmed submit must not desync local state.
        self.rpc
            .await_confirmation(&self.anchor, before, CONFIRM_TIMEOUT_SECS)?;
        self.inner.lock().unwrap().1 = root;
        Ok(res)
    }
}

impl<R: TonRpc> RecordAnchor for TonRecordAnchor<R> {
    fn append(&self, record: &JobRecord) {
        let job = JobId(record.job_id.clone());
        self.inner.lock().unwrap().0.push((job, record.leaf()));
    }
    fn epoch_root(&self) -> Hash32 {
        let g = self.inner.lock().unwrap();
        let leaves: Vec<Hash32> = g.0.iter().map(|(_, h)| *h).collect();
        crate::merkle::merkle_root(&leaves)
    }
    fn prove_inclusion(&self, job: &JobId) -> Option<InclusionProof> {
        let g = self.inner.lock().unwrap();
        let idx = g.0.iter().position(|(j, _)| j == job)?;
        let leaves: Vec<Hash32> = g.0.iter().map(|(_, h)| *h).collect();
        crate::merkle::build_proof(&leaves, idx)
    }
}

// ---------------------------------------------------------------------------
// Live toncenter transport (feature `ton-live`).
//
// Reads seqno + get-methods and BROADCASTS signed wallet-v5r1 external messages
// via toncenter `sendBoc`. Transport is `curl` (no async runtime / HTTP crate),
// matching the read-only seam already used by `tests/testnet_live.rs`. Disabled
// by default so the crate builds + unit-tests with zero network dependency.
// ---------------------------------------------------------------------------

/// Live toncenter RPC that signs with a wallet **v5r1** key and self-broadcasts.
#[cfg(feature = "ton-live")]
pub struct ToncenterRpc {
    rpc: String,
    api_key: Option<String>,
    wallet: crate::wallet::WalletV5R1,
    key: crate::wallet::WalletKey,
}

#[cfg(feature = "ton-live")]
impl ToncenterRpc {
    /// Build from resolved wiring + the wallet mnemonic. `network` selects the
    /// v5r1 wallet_id (testnet vs mainnet ⇒ distinct addresses).
    pub fn new(
        rpc_endpoint: &str,
        api_key: Option<String>,
        network: &str,
        mnemonic: &str,
    ) -> Result<Self, SettleError> {
        let key = crate::wallet::WalletKey::from_mnemonic(mnemonic)?;
        let global_id = if network.eq_ignore_ascii_case("mainnet") {
            crate::wallet::GLOBAL_ID_MAINNET
        } else {
            crate::wallet::GLOBAL_ID_TESTNET
        };
        let wallet = crate::wallet::WalletV5R1::new(key.public_key(), global_id);
        Ok(Self {
            rpc: rpc_endpoint.trim_end_matches('/').to_string(),
            api_key,
            wallet,
            key,
        })
    }

    /// The signer's own wallet address (message source / seqno target).
    pub fn wallet_address(&self) -> WalletAddress {
        self.wallet.address()
    }

    fn curl_post(&self, path: &str, json_body: &str) -> Result<String, SettleError> {
        let url = format!("{}/{}", self.rpc, path);
        let mut cmd = std::process::Command::new("curl");
        cmd.arg("-s")
            .arg("--max-time")
            .arg("30")
            .arg("-H")
            .arg("Content-Type: application/json");
        if let Some(k) = &self.api_key {
            cmd.arg("-H").arg(format!("X-API-Key: {k}"));
        }
        cmd.arg("-d").arg(json_body).arg(url);
        let out = cmd
            .output()
            .map_err(|e| SettleError::Backend(format!("curl spawn: {e}")))?;
        if !out.status.success() {
            return Err(SettleError::Backend(format!(
                "curl exit {:?}",
                out.status.code()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn read_seqno(&self) -> Result<u32, SettleError> {
        // A not-yet-deployed wallet has no seqno method ⇒ treat as 0 (deploy).
        match self.run_get_int(&self.wallet.address(), "seqno") {
            Ok(v) if v >= 0 => Ok(v as u32),
            _ => Ok(0),
        }
    }

    fn submit(
        &self,
        to: &WalletAddress,
        amount: Amount,
        body: &MessageBody,
        state_init: Option<Cell>,
    ) -> Result<String, SettleError> {
        let seqno = self.read_seqno()?;
        let valid_until = now_secs_u32() + 90;
        let msg = crate::wallet::InternalMessage {
            dest: *to,
            value: amount,
            body: body.cell.clone(),
            state_init,
            mode: 3,
        };
        let boc = crate::wallet::build_signed_external_v5r1(
            &self.wallet,
            &self.key,
            seqno,
            valid_until,
            &[msg],
        )?;
        let b64 = crate::wallet::base64_encode(&boc);
        let json = format!(r#"{{"boc":"{b64}"}}"#);
        let raw = self.curl_post("sendBoc", &json)?;
        let v: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|e| SettleError::Backend(format!("bad sendBoc JSON: {e}")))?;
        if v.get("ok").and_then(|o| o.as_bool()) == Some(true) {
            // toncenter returns {"ok":true,"result":{"hash":"..."}} (hash varies).
            let hash = v
                .get("result")
                .and_then(|r| r.get("hash"))
                .and_then(|h| h.as_str())
                .unwrap_or("ok")
                .to_string();
            Ok(hash)
        } else {
            Err(SettleError::Backend(format!("sendBoc rejected: {raw}")))
        }
    }

    /// Most recent transaction on `account`: its `lt`, and — *if* the endpoint
    /// exposes the transaction description (toncenter v3 / TON API; v2 omits it) —
    /// a `Some(exit_code)` when that transaction FAILED (compute/action phase or
    /// aborted). `Ok(None)` when the account has no transactions yet (e.g. a
    /// not-yet-deployed escrow). Used by the confirmation poll.
    fn fetch_latest_tx(
        &self,
        account: &WalletAddress,
    ) -> Result<Option<(u64, TxOutcome)>, SettleError> {
        let json = format!(r#"{{"address":"{}","limit":1}}"#, account.to_raw_string());
        let raw = self.curl_post("getTransactions", &json)?;
        let v: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|e| SettleError::Backend(format!("bad getTransactions JSON: {e}")))?;
        // An uninitialized account yields ok:false / an empty list — no txs yet.
        if v.get("ok").and_then(|o| o.as_bool()) == Some(false) {
            return Ok(None);
        }
        let arr = match v.get("result").and_then(|r| r.as_array()) {
            Some(a) if !a.is_empty() => a,
            _ => return Ok(None),
        };
        let tx = &arr[0];
        let lt_s = tx
            .get("transaction_id")
            .and_then(|t| t.get("lt"))
            .and_then(|l| l.as_str())
            .ok_or_else(|| SettleError::Backend(format!("no tx lt: {raw}")))?;
        let lt = lt_s
            .parse::<u64>()
            .map_err(|e| SettleError::Backend(format!("parse lt '{lt_s}': {e}")))?;
        Ok(Some((lt, tx_outcome(tx))))
    }
}

/// The execution outcome of a landed transaction, as far as the RPC endpoint
/// lets us determine it. `Unknown` is DISTINCT from `Succeeded`: a toncenter v2
/// `getTransactions` response omits the `description`, so a landed transaction
/// could have aborted without us seeing it. We must never report settlement
/// success on an `Unknown` outcome (H4: a failed settle would otherwise be
/// recorded as paid).
#[cfg(feature = "ton-live")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TxOutcome {
    /// The transaction ran and FAILED (non-zero compute/action phase, or aborted).
    Failed(i32),
    /// The endpoint exposed a `description` and it shows the transaction succeeded.
    Succeeded,
    /// The endpoint did NOT expose an execution outcome (no `description`) — we
    /// cannot tell success from failure and must not assume either.
    Unknown,
}

/// Classify a transaction's execution outcome from its `description`. A non-zero
/// compute-phase `exit_code`, a non-zero action-phase `result_code`, or
/// `aborted` ⇒ [`TxOutcome::Failed`]. A present description with none of those ⇒
/// [`TxOutcome::Succeeded`]. An ABSENT description (toncenter v2 omits it) ⇒
/// [`TxOutcome::Unknown`] — we cannot tell success from failure, so the caller
/// must NOT treat it as confirmation (the old code returned `None` here and a
/// landed-but-failed tx was silently accepted as success).
#[cfg(feature = "ton-live")]
fn tx_outcome(tx: &serde_json::Value) -> TxOutcome {
    let Some(desc) = tx.get("description") else {
        return TxOutcome::Unknown;
    };
    let compute = desc
        .get("compute_ph")
        .and_then(|c| c.get("exit_code"))
        .and_then(|x| x.as_i64());
    if let Some(c) = compute {
        if c != 0 {
            return TxOutcome::Failed(c as i32);
        }
    }
    let action = desc
        .get("action")
        .and_then(|a| a.get("result_code"))
        .and_then(|x| x.as_i64());
    if let Some(rc) = action {
        if rc != 0 {
            return TxOutcome::Failed(rc as i32);
        }
    }
    if desc.get("aborted").and_then(|a| a.as_bool()) == Some(true) {
        return TxOutcome::Failed(compute.unwrap_or(-1) as i32);
    }
    TxOutcome::Succeeded
}

#[cfg(feature = "ton-live")]
impl TonRpc for ToncenterRpc {
    fn send_internal(
        &self,
        to: &WalletAddress,
        amount: Amount,
        body: &MessageBody,
    ) -> Result<String, SettleError> {
        self.submit(to, amount, body, None)
    }

    fn deploy(
        &self,
        to: &WalletAddress,
        amount: Amount,
        state_init: &Cell,
        body: &MessageBody,
    ) -> Result<String, SettleError> {
        self.submit(to, amount, body, Some(state_init.clone()))
    }

    fn run_get_int(&self, addr: &WalletAddress, method: &str) -> Result<i128, SettleError> {
        let json = format!(
            r#"{{"address":"{}","method":"{method}","stack":[]}}"#,
            addr.to_raw_string()
        );
        let raw = self.curl_post("runGetMethod", &json)?;
        let v: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|e| SettleError::Backend(format!("bad JSON: {e}")))?;
        if v.get("ok").and_then(|o| o.as_bool()) == Some(false) {
            return Err(SettleError::Backend(format!("toncenter error: {raw}")));
        }
        let s = v
            .get("result")
            .and_then(|r| r.get("stack"))
            .and_then(|s| s.get(0))
            .and_then(|e| e.get(1))
            .and_then(|x| x.as_str())
            .ok_or_else(|| SettleError::Backend(format!("no stack[0]: {raw}")))?
            .trim()
            .to_string();
        let parsed = if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            i128::from_str_radix(h, 16)
        } else if let Some(h) = s.strip_prefix("-0x") {
            i128::from_str_radix(h, 16).map(|n| -n)
        } else {
            s.parse::<i128>()
        };
        parsed.map_err(|e| SettleError::Backend(format!("parse '{s}': {e}")))
    }

    fn run_get_anchor_state(
        &self,
        addr: &WalletAddress,
    ) -> Result<Option<(u32, Hash32)>, SettleError> {
        // get_anchor_state stack: [currentEpoch, lastRoot, nextDisputeId]. We need
        // the first two items; the single-int seam only exposes the first.
        let json = format!(
            r#"{{"address":"{}","method":"get_anchor_state","stack":[]}}"#,
            addr.to_raw_string()
        );
        let raw = self.curl_post("runGetMethod", &json)?;
        let v: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|e| SettleError::Backend(format!("bad JSON: {e}")))?;
        if v.get("ok").and_then(|o| o.as_bool()) == Some(false) {
            return Err(SettleError::Backend(format!("toncenter error: {raw}")));
        }
        let stack = v
            .get("result")
            .and_then(|r| r.get("stack"))
            .and_then(|s| s.as_array())
            .ok_or_else(|| SettleError::Backend(format!("no stack: {raw}")))?;
        let item = |i: usize| -> Option<&str> { stack.get(i)?.get(1)?.as_str().map(|x| x.trim()) };
        let epoch_s =
            item(0).ok_or_else(|| SettleError::Backend(format!("no currentEpoch: {raw}")))?;
        let epoch = u32::from_str_radix(
            epoch_s.trim_start_matches("0x").trim_start_matches("0X"),
            16,
        )
        .or_else(|_| epoch_s.parse::<u32>())
        .map_err(|e| SettleError::Backend(format!("parse currentEpoch '{epoch_s}': {e}")))?;
        let root_s = item(1).ok_or_else(|| SettleError::Backend(format!("no lastRoot: {raw}")))?;
        let mut root = [0u8; 32];
        let hex_digits = root_s.trim_start_matches("0x").trim_start_matches("0X");
        // Left-pad to 64 nibbles (a small / leading-zero root prints short).
        let padded = format!("{hex_digits:0>64}");
        let bytes = hex::decode(&padded)
            .map_err(|e| SettleError::Backend(format!("parse lastRoot '{root_s}': {e}")))?;
        if bytes.len() != 32 {
            return Err(SettleError::Backend(format!(
                "lastRoot not 32 bytes: {root_s}"
            )));
        }
        root.copy_from_slice(&bytes);
        Ok(Some((epoch, root)))
    }

    fn run_get_address(
        &self,
        addr: &WalletAddress,
        method: &str,
    ) -> Result<Option<WalletAddress>, SettleError> {
        // An address getter returns a `MsgAddress` slice on the stack: a cell BoC
        // (`["cell", {"bytes": "<base64>"}]`). Decode the cell and parse the address.
        let json = format!(
            r#"{{"address":"{}","method":"{method}","stack":[]}}"#,
            addr.to_raw_string()
        );
        let raw = self.curl_post("runGetMethod", &json)?;
        let v: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|e| SettleError::Backend(format!("bad JSON: {e}")))?;
        if v.get("ok").and_then(|o| o.as_bool()) == Some(false) {
            return Err(SettleError::Backend(format!("toncenter error: {raw}")));
        }
        let entry = v
            .get("result")
            .and_then(|r| r.get("stack"))
            .and_then(|s| s.get(0))
            .and_then(|e| e.get(1))
            .ok_or_else(|| SettleError::Backend(format!("no stack[0] for {method}: {raw}")))?;
        // The slice/cell payload carries the BoC under `.bytes` (base64).
        let b64 = entry.get("bytes").and_then(|x| x.as_str()).ok_or_else(|| {
            SettleError::Backend(format!("address getter has no cell bytes: {raw}"))
        })?;
        let boc = crate::wallet::base64_decode(b64.trim())
            .ok_or_else(|| SettleError::Backend("address cell: invalid base64 BoC".into()))?;
        let cell = Cell::from_boc(&boc).ok_or_else(|| {
            // On-chain troubleshooting: the RPC returned a BoC the hardened parser
            // rejected (truncated/oversized/cyclic/over-deep/bad-CRC) — surface the
            // size so a malformed or hostile endpoint response is diagnosable.
            tracing::warn!(
                bytes = boc.len(),
                "RPC get-method returned a BoC that failed to parse (malformed address cell)"
            );
            SettleError::Backend("address cell: BoC failed to parse".into())
        })?;
        let parsed = cell
            .parser()
            .load_address()
            .ok_or_else(|| SettleError::Backend("address cell: not a MsgAddress".into()))?;
        Ok(Some(parsed))
    }

    fn last_tx_lt(&self, account: &WalletAddress) -> Result<u64, SettleError> {
        Ok(self
            .fetch_latest_tx(account)?
            .map(|(lt, _)| lt)
            .unwrap_or(0))
    }

    fn await_confirmation(
        &self,
        account: &WalletAddress,
        after_lt: u64,
        deadline_secs: u64,
    ) -> Result<(), SettleError> {
        let deadline = now_secs_u32() as u64 + deadline_secs;
        loop {
            if let Some((lt, outcome)) = self.fetch_latest_tx(account)? {
                if lt > after_lt {
                    // A newer transaction landed on the destination. Surface a
                    // reported failure; treat a verified success as confirmation;
                    // and REFUSE to report success when the endpoint hides the
                    // outcome (H4) — reporting "paid" on an unverifiable result is
                    // worse than failing loudly. Polling longer cannot make a
                    // description appear, so fail immediately with guidance.
                    return match outcome {
                        TxOutcome::Failed(exit_code) => Err(SettleError::TxFailed { exit_code }),
                        TxOutcome::Succeeded => Ok(()),
                        TxOutcome::Unknown => Err(SettleError::Backend(
                            "a newer transaction landed but the RPC endpoint does not expose its \
                             execution outcome (no `description`/exit code — e.g. toncenter v2 \
                             `getTransactions`). Refusing to confirm an unverifiable result; \
                             configure a description-capable endpoint (toncenter v3 / TON API) \
                             for on-chain settlement."
                                .into(),
                        )),
                    };
                }
            }
            if now_secs_u32() as u64 >= deadline {
                // Nothing newer than `after_lt` landed in time: the external
                // message was likely dropped (bad sig / replay / expired /
                // under-funded) or is still in flight. Do NOT assume success.
                return Err(SettleError::TxUnconfirmed);
            }
            std::thread::sleep(std::time::Duration::from_secs(3));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn message_bodies_carry_the_contract_opcodes() {
        assert_eq!(build_stake_deposit(1, 100).opcode, OP_STAKE_DEPOSIT);
        assert_eq!(
            &build_stake_deposit(1, 100).bytes[0..4],
            &OP_STAKE_DEPOSIT.to_be_bytes()
        );

        let challenger = WalletAddress::new(0, [7u8; 32]);
        let slash = build_stake_slash(9, 50, SlashReason::Cheat, &challenger);
        assert_eq!(slash.opcode, OP_STAKE_SLASH);
        // opcode(4) + queryId(8) + coins(16) + reason(1) + addr(36)
        assert_eq!(slash.bytes.len(), 4 + 8 + 16 + 1 + 36);
        // reason byte is the SlashReason discriminant.
        assert_eq!(slash.bytes[4 + 8 + 16], SlashReason::Cheat.code());
    }

    fn sample_params() -> GlobalParams {
        GlobalParams {
            platform_fee_bps: 200,
            surcharge_bps: 500,
            participation_commission_bps: 200,
            slash_wrong_bps: 1500,
            slash_cheat_bps: 10000,
            slash_downtime_bps: 200,
            slash_equivocation_bps: 5000,
            split_challenger_bps: 4000,
            split_redundancy_bps: 3000,
            split_burn_bps: 2000,
            split_treasury_bps: 1000,
            min_stake: 0,
            min_stake_internal: 100,
            min_stake_sensitive: 1000,
            stake_cap: 100_000,
            unbonding_secs: 604_800,
            challenge_window_secs: 86_400,
            n_public: 3,
            n_default: 5,
            n_max: 10,
            quorum: 3,
            checksum_min: 3,
            w_quality_bps: 6000,
            w_stake_bps: 1500,
            w_price_bps: 2500,
            slash_failed_commitment_bps: 1000,
            attempt_deadline_ms: 60_000,
            progress_interval_ms: 2_000,
            progress_stall_mult: 5,
        }
    }

    #[test]
    fn update_params_body_carries_opcode_and_validates() {
        let fee = WalletAddress::new(0, [9u8; 32]);
        let b = build_update_params(7, &fee, &sample_params());
        assert_eq!(b.opcode, OP_UPDATE_PARAMS);
        assert_eq!(&b.bytes[0..4], &OP_UPDATE_PARAMS.to_be_bytes());
        // opcode(4)+queryId(8)+addr(36)+11*u16(22)+4*coins(64)+2*u32(8)+5*u8(5)+3*u16(6)
        // + resilience: u16(2)+u32(4)+u32(4)+u8(1) = 11 more bytes.
        assert_eq!(
            b.bytes.len(),
            4 + 8 + 36 + 22 + 64 + 8 + 5 + 6 + (2 + 4 + 4 + 1)
        );
        sample_params().validate().unwrap();
    }

    #[test]
    fn global_params_validate_rejects_bad_split_and_ordering() {
        let mut p = sample_params();
        p.split_burn_bps = 5000; // sum != 10000
        assert!(p.validate().is_err());
        let mut p = sample_params();
        p.unbonding_secs = 1; // < challenge window
        assert!(p.validate().is_err());
        let mut p = sample_params();
        p.n_max = 2; // < n_default
        assert!(p.validate().is_err());
    }

    #[test]
    fn update_admin_body_layout() {
        let admin = WalletAddress::new(0, [1u8; 32]);
        let b = build_update_admin(1, &admin);
        assert_eq!(b.opcode, OP_UPDATE_ADMIN);
        assert_eq!(b.bytes.len(), 4 + 8 + 36);
    }

    #[test]
    fn upgrade_code_bodies_carry_opcodes_and_ref_the_new_code() {
        // A stand-in "new code" cell tree (multi-cell) the upgrade carries by ref.
        let new_code = CellBuilder::new()
            .store_uint(0xC0DE, 16)
            .store_ref(CellBuilder::new().store_uint(0xBEEF, 16).build())
            .build();

        // GlobalParams admin upgrade_code: opcode + queryId inline, code as ^ref.
        let gp = build_upgrade_code(7, &new_code);
        assert_eq!(gp.opcode, OP_UPGRADE_CODE);
        assert_eq!(gp.opcode, 0x4750_4104);
        assert_eq!(&gp.bytes[0..4], &OP_UPGRADE_CODE.to_be_bytes());
        assert_eq!(gp.bytes.len(), 4 + 8); // flat ABI: opcode + queryId (code is a ref)
        assert_eq!(gp.cell.refs().len(), 1, "new code lives in a ^child cell");
        assert_eq!(gp.cell.refs()[0].repr_hash(), new_code.repr_hash());

        // RecordAnchor authority upgrade_code: same shape, distinct opcode.
        let an = build_anchor_upgrade_code(8, &new_code);
        assert_eq!(an.opcode, OP_ANCHOR_UPGRADE_CODE);
        assert_eq!(an.opcode, 0x414e_4304);
        assert_eq!(an.bytes.len(), 4 + 8);
        assert_eq!(an.cell.refs().len(), 1);
    }

    #[test]
    fn stake_timelocked_upgrade_bodies_layout() {
        // Announce carries the announced code's 256-bit cell hash inline.
        let code_hash = [0x5Au8; 32];
        let ann = build_stake_announce_upgrade(1, &code_hash);
        assert_eq!(ann.opcode, OP_STAKE_ANNOUNCE_UPGRADE);
        assert_eq!(ann.opcode, 0x534b_4b07);
        // opcode(4) + queryId(8) + hash(32)
        assert_eq!(ann.bytes.len(), 4 + 8 + 32);
        assert_eq!(&ann.bytes[4 + 8..], &code_hash);

        // Apply carries the successor code by ref (its hash must match announce).
        let new_code = CellBuilder::new().store_uint(0xABCD, 16).build();
        let apply = build_stake_apply_upgrade(2, &new_code);
        assert_eq!(apply.opcode, OP_STAKE_APPLY_UPGRADE);
        assert_eq!(apply.opcode, 0x534b_4b08);
        assert_eq!(apply.bytes.len(), 4 + 8);
        assert_eq!(apply.cell.refs().len(), 1);
        assert_eq!(apply.cell.refs()[0].repr_hash(), new_code.repr_hash());

        // Cancel is just opcode + queryId.
        let canc = build_stake_cancel_upgrade(3);
        assert_eq!(canc.opcode, OP_STAKE_CANCEL_UPGRADE);
        assert_eq!(canc.opcode, 0x534b_4b09);
        assert_eq!(canc.bytes.len(), 4 + 8);
    }

    #[test]
    fn global_params_client_reads_code_version() {
        let mut values = HashMap::new();
        values.insert("get_code_version".to_string(), 3i128);
        let client = GlobalParamsClient::new(
            GetMethodRpc {
                values,
                ..Default::default()
            },
            WalletAddress::new(0, [0xCD; 32]),
        );
        assert_eq!(client.code_version().unwrap(), 3);
    }

    #[test]
    fn escrow_settle_body_layout_is_stable() {
        let winner = WalletAddress::new(0, [2u8; 32]);
        let b = build_escrow_settle(1, &[3u8; 32], &winner, 60, 60, 2, &[], &[winner]);
        assert_eq!(b.opcode, OP_ESCROW_SETTLE);
        // opcode(4)+queryId(8)+hash(32)+addr(36)+coins(16 base)+coins(16 winnerAmount)
        // +coins(16 platformFee) — the flat ABI omits BOTH the participants dict and
        // the candidates dict (they live only in the live cell).
        assert_eq!(b.bytes.len(), 4 + 8 + 32 + 36 + 16 + 16 + 16);
    }

    // -- participants + candidates dicts, pinned to the Acton emulator ------
    // Reference body-cell hashes come from `ton/scripts/_probe_dict.tolk`, which
    // builds the IDENTICAL `EscrowSettle { ..., participants: map<address,coins>,
    // candidates: map<address,uint1> }` on-chain (Tolk `.toCell()`) and prints
    // `cell.hash()`. The probe sets `candidates = {winner} ∪ {participant keys}`,
    // so these tests build the SAME candidate set. If the off-chain HashmapE
    // encoding (267-bit addr keys, hml labels, coins / uint1 leaves) drifts from
    // TON's, these break — so the two dicts the contract reads are byte-identical.

    fn probe_result_hash() -> [u8; 32] {
        let mut rh = [0u8; 32];
        rh[24..32].copy_from_slice(&0xABCDEF0123456789u64.to_be_bytes());
        rh
    }
    fn probe_winner() -> WalletAddress {
        WalletAddress::new(0, [0x02u8; 32])
    }
    /// The candidate set the probe builds: the winner plus each participant key.
    fn probe_candidates(winner: &WalletAddress, participants: &[Payout]) -> Vec<WalletAddress> {
        let mut c = vec![*winner];
        for p in participants {
            if !c.contains(&p.to) {
                c.push(p.to);
            }
        }
        c
    }

    #[test]
    fn escrow_settle_participants_dict_matches_onchain_hashmap() {
        let rh = probe_result_hash();
        let w = probe_winner();
        let p = |b: u8, amt: Amount| Payout {
            to: WalletAddress::new(0, [b; 32]),
            amount: amt,
        };
        let settle = |parts: &[Payout]| {
            build_escrow_settle(1, &rh, &w, 60, 60, 2, parts, &probe_candidates(&w, parts))
        };

        // queryId=1, winnerAmount=60, platformFee=2 — same scalars as the probe.
        // Pinned to `_probe_dict.tolk` (EMPTY/ONE/TWO/THREE `*_HASH`) AFTER the B1
        // `candidates` field was appended to `EscrowSettle`.
        let empty = settle(&[]);
        assert_eq!(
            hex::encode(empty.cell.repr_hash()),
            "10d43d377a772168a10d2817f5b40c168c8b86c4dc386824738eeac625ef47c2"
        );

        let one = settle(&[p(0x11, 1)]);
        assert_eq!(
            hex::encode(one.cell.repr_hash()),
            "70e5b1d4a492dce124f0eb2b5d4efd6ce153ff1cc8d7f50d7ea925941dd54ae8"
        );

        let two = settle(&[p(0x11, 1), p(0x22, 256)]);
        assert_eq!(
            hex::encode(two.cell.repr_hash()),
            "59e4b1b7896267f339ee7a009cf531824913e5739432d827ad9b838f91380841"
        );

        let three = settle(&[p(0x11, 1), p(0x22, 256), p(0x33, 0xDEAD)]);
        assert_eq!(
            hex::encode(three.cell.repr_hash()),
            "5c7db6b1f275e18897cfb43127729e368009069cee691faf12882d16444d3b89"
        );
    }

    #[test]
    fn escrow_settle_dict_is_order_independent_and_merges_dupes() {
        let rh = probe_result_hash();
        let w = probe_winner();
        let p = |b: u8, amt: Amount| Payout {
            to: WalletAddress::new(0, [b; 32]),
            amount: amt,
        };
        let cands = [
            w,
            WalletAddress::new(0, [0x11; 32]),
            WalletAddress::new(0, [0x22; 32]),
        ];
        // Entry order does not change the canonical dict (hence the cell hash) for
        // EITHER the participants OR the candidates map.
        let ab = build_escrow_settle(1, &rh, &w, 60, 60, 2, &[p(0x11, 1), p(0x22, 256)], &cands);
        let ba = build_escrow_settle(
            1,
            &rh,
            &w,
            60,
            60,
            2,
            &[p(0x22, 256), p(0x11, 1)],
            &[cands[2], cands[0], cands[1]],
        );
        assert_eq!(ab.cell.repr_hash(), ba.cell.repr_hash());
        // Duplicate payout wallets merge (summed) — a map key is unique. Two
        // 0x11→128 entries equal one 0x11→256 entry. Duplicate candidates collapse.
        let dup = build_escrow_settle(
            1,
            &rh,
            &w,
            60,
            60,
            2,
            &[p(0x11, 128), p(0x11, 128)],
            &[w, cands[1], cands[1]],
        );
        let single = build_escrow_settle(1, &rh, &w, 60, 60, 2, &[p(0x11, 256)], &[w, cands[1]]);
        assert_eq!(dup.cell.repr_hash(), single.cell.repr_hash());
        // Zero-amount participants are dropped (no leaf), matching an empty dict;
        // with the SAME candidate set the two bodies hash identically.
        let zero = build_escrow_settle(1, &rh, &w, 60, 60, 2, &[p(0x44, 0)], &[w]);
        let empty = build_escrow_settle(1, &rh, &w, 60, 60, 2, &[], &[w]);
        assert_eq!(zero.cell.repr_hash(), empty.cell.repr_hash());
    }

    #[test]
    fn candidates_commitment_matches_onchain() {
        // `candidates_commitment` must byte-match the on-chain `candidatesCommitment`
        // (ton/scripts/_probe_v2.tolk): cellhash(beginCell().storeDict(c).endCell())
        // over the SAME map<address, uint1>.
        let winner = WalletAddress::new(0, [0x02u8; 32]);
        let key1 = WalletAddress::new(0, [0x11u8; 32]);
        // {winner, KEY1} — CANDIDATES_COMMITMENT_winnerKey1.
        assert_eq!(
            hex::encode(candidates_commitment(&[winner, key1])),
            "b49327a82234164593ebbc61e44d83a02ddcd95a2c5ba1ed14bfe5f86cccc80d"
        );
        // Order-independent + duplicate-collapsing (canonical dict).
        assert_eq!(
            candidates_commitment(&[winner, key1]),
            candidates_commitment(&[key1, winner])
        );
        assert_eq!(
            candidates_commitment(&[winner, key1, winner]),
            candidates_commitment(&[winner, key1])
        );
        // Empty set ⇒ the empty-dict commitment — CANDIDATES_COMMITMENT_empty.
        assert_eq!(
            hex::encode(candidates_commitment(&[])),
            "90aec8965afabb16ebc3cb9b408ebae71b618d78788bc80d09843593cac98da4"
        );
    }

    /// The compiled `JobEscrow` code BoC (`code_boc64`) from
    /// `ton/build/JobEscrow.json`, with its expected code-cell repr-hash. Loading
    /// it proves the live escrow-code wiring consumes the REAL compiled contract
    /// (multi-cell BoC), and pins the StateInit derivation to the deployed code.
    const JOB_ESCROW_CODE_B64: &str = "te6ccgECCwEAAbcAART/APSkE/S88sgLAQIBYgIDAc7Q+JGRMOAg1ywiKpoYFOMC1ywiKpoYHI47W+1E0PpI+kj6ANMf0gAB8tDg+CMivvLg4STI+lIU+lJY+gLLH8+DzsntVMjPhQj6UnDPC27JgQCC+wDg1ywiKpoYBDGRMOCEDwHHAPL0BAIBIAcIAvgx7UTQ+kj6SPoA1h/SACDXTPiSJscF8uDeAvLQ4AHQ+kjT/9EH0z8x0//6SPoA+gD0BVBLuvLg33AqgQEL9IJvpZCfAfoA0RKgURuBAQv0dG+l6FtTE6CgUwe78uDiKcj6Uhn6Uif6AhbOz4MUzsntVCPCAJJsIuMNIMIABQYAJsjPhQgT+lJQA/oCcM8Laslz+wAA0o4SyM+FCBL6UgH6AnDPC2rJc/sAkVviI4EBC/SCb6WQjihSAvoA0SDCAI4SyM+FCBP6Ulj6AnDPC2rJc/sAkjAx4iSBAQv0dG+l6FszEqEgwgCOEsjPhQgS+lIB+gJwzwtqyXP7AJFb4gAdvJ7HaiaGumaH0kGOn/6MAgFiCQoAEbF3u1E0PpIMIAAlsp07UTQ+kgx+kgx+gDTH9cKAIA==";

    #[test]
    fn job_escrow_code_boc_parses_and_matches_artifact_hash() {
        let code =
            escrow_code_from_boc_base64(JOB_ESCROW_CODE_B64).expect("artifact code BoC parses");
        assert_eq!(
            hex::encode_upper(code.repr_hash()),
            "60C9F21AA3146CFA0A77B28098865118080B67F9CDF700F7C5DEDC984BD77E71"
        );
    }

    #[test]
    fn escrow_terms_cell_layout() {
        // EscrowTerms = treasury(addr 267) + expectedHash(uint256) +
        // candidatesHash(uint256) + paramsVersion(uint32) + platformFeeBps(uint16) +
        // participationCommissionBps(uint16). All six round-trip in field order —
        // `platformFeeBps` (φ) then `participationCommissionBps` (κ) are appended
        // last (the fee/commission enforcement fields bound at open).
        let treasury = WalletAddress::new(0, [0x7eu8; 32]);
        let expected = [0xABu8; 32];
        let cand = [0xCDu8; 32];
        let terms = build_escrow_terms(&treasury, &expected, &cand, 7, 1500, 500);
        let mut p = terms.parser();
        assert_eq!(p.load_address(), Some(treasury));
        assert_eq!(p.load_bits(256).unwrap(), expected.to_vec());
        assert_eq!(p.load_bits(256).unwrap(), cand.to_vec());
        assert_eq!(p.load_uint(32).unwrap(), 7);
        assert_eq!(p.load_uint(16).unwrap(), 1500);
        assert_eq!(p.load_uint(16).unwrap(), 500);
    }

    #[test]
    fn escrow_terms_cell_matches_onchain() {
        // Pin the v3 EscrowTerms cell hash to the Acton emulator
        // (ton/scripts/_probe_v2.tolk): treasury=0x7e.., expectedHash=0xabab..,
        // candidatesHash = commitment over {winner 0x02.., KEY1 0x11..},
        // paramsVersion = 7, platformFeeBps = 1500, participationCommissionBps = 500.
        // Proves the 6-field terms layout (hence the escrow deterministic address)
        // is byte-identical to the contract's `.toCell()`.
        let treasury = WalletAddress::new(0, [0x7eu8; 32]);
        let expected = [0xABu8; 32];
        let winner = WalletAddress::new(0, [0x02u8; 32]);
        let key1 = WalletAddress::new(0, [0x11u8; 32]);
        let cand = candidates_commitment(&[winner, key1]);
        let terms = build_escrow_terms(&treasury, &expected, &cand, 7, 1500, 500);
        assert_eq!(
            hex::encode(terms.repr_hash()),
            "05a993fe1d08b2327a830a1b99caca4f879ec9a2517ca76201c04571bc299a6c"
        );
        // candidatesHash = 0 (unbound), same other fields — ESCROW_TERMS_V3_UNBOUND.
        let unbound = build_escrow_terms(&treasury, &expected, &[0u8; 32], 7, 1500, 500);
        assert_eq!(
            hex::encode(unbound.repr_hash()),
            "28b75b0f8c393984a50d35cd7888bf55a505c7e165b57b64393272da7f036f33"
        );
    }

    #[test]
    fn escrow_data_state_init_matches_onchain() {
        // Pin the v2 EscrowStorage StateInit (with the trailing empty `pending`
        // map) to the Acton emulator (ton/scripts/_probe_v2.tolk, ESCROW_SI_V2),
        // using the SAME stand-in code (0xC0DE), bound terms from
        // `escrow_terms_cell_matches_onchain`, requester=KEY1, arbiter=winner,
        // escrowAmount=1000, deadline=42.
        let code = CellBuilder::new().store_uint(0xC0DE, 16).build();
        let treasury = WalletAddress::new(0, [0x7eu8; 32]);
        let expected = [0xABu8; 32];
        let winner = WalletAddress::new(0, [0x02u8; 32]);
        let key1 = WalletAddress::new(0, [0x11u8; 32]);
        let cand = candidates_commitment(&[winner, key1]);
        let terms = build_escrow_terms(&treasury, &expected, &cand, 7, 1500, 500);
        let init = EscrowInit {
            requester: key1,
            arbiter: winner,
            escrow_amount: 1000,
            deadline: 42,
        };
        let data = build_escrow_data(&init, terms);
        let addr = WalletAddress::from_state_init(BASECHAIN, &code, &data);
        assert_eq!(
            hex::encode(addr.hash),
            "5af54256b23facb2d12bd4ba9df2876a99dfeb8aac4121f659301c226c160b6d"
        );
    }

    #[test]
    fn anchor_open_dispute_drops_claimed_root_and_chains_proof() {
        // A2: the hardened AnchorOpenDispute is `queryId, epoch, leaf, proof` — NO
        // claimedRoot. The flat ABI keeps the scalar prefix inline; `proof` is a
        // `Maybe ^MerkleStep` on the live cell.
        let leaf = [0x5Au8; 32];
        // A 2-step proof: sibling on the RIGHT (dir=0), then LEFT (dir=1).
        let proof = InclusionProof {
            leaf,
            siblings: vec![(false, [0x11u8; 32]), (true, [0x22u8; 32])],
        };
        let d = build_anchor_open_dispute(7, 3, &leaf, &proof);
        assert_eq!(d.opcode, OP_ANCHOR_OPEN_DISPUTE);
        assert_eq!(d.opcode, 0x414e_4302);
        // flat ABI: opcode(4) + queryId(8) + epoch(4) + leaf(32) (proof is a ref).
        assert_eq!(d.bytes.len(), 4 + 8 + 4 + 32);
        // Live cell carries the proof chain as a single Maybe ^MerkleStep ref.
        assert_eq!(
            d.cell.refs().len(),
            1,
            "proof chain lives in a ^MerkleStep ref"
        );

        // The chain nests innermost-last: outer step is the FIRST sibling (dir=0,
        // sibling 0x11), whose `next` ref is the second step (dir=1, sibling 0x22),
        // whose `next` is absent.
        let step0 = &d.cell.refs()[0];
        let mut s0 = step0.parser();
        assert_eq!(s0.load_uint(1), Some(0)); // dir = 0 (sibling on the right)
        assert_eq!(s0.load_bits(256).unwrap(), [0x11u8; 32].to_vec());
        assert_eq!(s0.load_uint(1), Some(1)); // Maybe present
        let step1 = s0.load_ref().expect("second step");
        let mut s1 = step1.parser();
        assert_eq!(s1.load_uint(1), Some(1)); // dir = 1 (sibling on the left)
        assert_eq!(s1.load_bits(256).unwrap(), [0x22u8; 32].to_vec());
        assert_eq!(s1.load_uint(1), Some(0)); // Maybe absent (end of chain)

        // An empty proof (single-leaf tree: leaf == root) ⇒ no proof ref.
        let none = build_anchor_open_dispute(
            8,
            1,
            &leaf,
            &InclusionProof {
                leaf,
                siblings: vec![],
            },
        );
        assert!(
            none.cell.refs().is_empty(),
            "absent proof ⇒ Maybe `0` bit, no ref"
        );
    }

    #[test]
    fn anchor_open_dispute_proof_chain_verifies_against_merkle() {
        // The proof cell the bridge builds must encode the SAME path the off-chain
        // verifier folds — i.e. round-trip a real multi-leaf proof and confirm the
        // dir/sibling order the contract walks is preserved.
        let leaves: Vec<Hash32> = (0u32..5)
            .map(|n| *blake3::hash(&n.to_le_bytes()).as_bytes())
            .collect();
        let root = crate::merkle::merkle_root(&leaves);
        for i in 0..leaves.len() {
            let proof = crate::merkle::build_proof(&leaves, i).unwrap();
            assert!(crate::merkle::verify_inclusion(&root, &proof));
            let cell = build_merkle_proof_cell(&proof);
            // The number of MerkleStep cells equals the sibling count.
            let mut depth = 0usize;
            let mut cur = cell.clone();
            while let Some(c) = cur {
                depth += 1;
                let mut pr = c.parser();
                let _dir = pr.load_uint(1);
                let _sib = pr.load_bits(256);
                // walk to next
                let present = pr.load_uint(1) == Some(1);
                cur = if present {
                    pr.load_ref().cloned()
                } else {
                    None
                };
            }
            assert_eq!(
                depth,
                proof.siblings.len(),
                "step count == sibling count for leaf {i}"
            );
        }
    }

    #[test]
    fn new_op_builders_carry_opcodes_and_layout() {
        // EscrowClaim: opcode + queryId + recipient(addr).
        let recip = WalletAddress::new(0, [0x09u8; 32]);
        let ec = build_escrow_claim(3, &recip);
        assert_eq!(ec.opcode, OP_ESCROW_CLAIM);
        assert_eq!(ec.opcode, 0x4553_4304);
        assert_eq!(ec.bytes.len(), 4 + 8 + 36);
        // StakeClaim: opcode + queryId + recipient(addr).
        let sc = build_stake_claim(4, &recip);
        assert_eq!(sc.opcode, OP_STAKE_CLAIM);
        assert_eq!(sc.opcode, 0x534b_4b0a);
        assert_eq!(sc.bytes.len(), 4 + 8 + 36);
        // GlobalParams AnnounceCode: opcode + queryId + newCodeHash(32).
        let hash = [0x7Bu8; 32];
        let ann = build_announce_code(5, &hash);
        assert_eq!(ann.opcode, OP_ANNOUNCE_CODE);
        assert_eq!(ann.opcode, 0x4750_4105);
        assert_eq!(ann.bytes.len(), 4 + 8 + 32);
        assert_eq!(&ann.bytes[4 + 8..], &hash);
        // GlobalParams CancelCode: opcode + queryId.
        let canc = build_cancel_code(6);
        assert_eq!(canc.opcode, OP_CANCEL_CODE);
        assert_eq!(canc.opcode, 0x4750_4106);
        assert_eq!(canc.bytes.len(), 4 + 8);
        // The outgoing-only payload op tags match the contracts.
        assert_eq!(OP_ESCROW_PAYOUT, 0x4553_4305);
        assert_eq!(OP_STAKE_PAYOUT, 0x534b_4b0b);
        assert_eq!(OP_ANCHOR_BOND_REFUND, 0x414e_4305);
    }

    #[test]
    fn query_id_gen_is_monotonic_and_deterministic() {
        let g = QueryIdGen::new();
        assert_eq!(g.next(), 1);
        assert_eq!(g.next(), 2);
        assert_eq!(g.next(), 3);
    }

    /// Recording fake transport for unit tests (no network).
    #[derive(Default)]
    struct RecordingRpc {
        sent: Mutex<Vec<MessageBody>>,
    }
    impl TonRpc for RecordingRpc {
        fn send_internal(
            &self,
            _to: &WalletAddress,
            _amount: Amount,
            body: &MessageBody,
        ) -> Result<String, SettleError> {
            self.sent.lock().unwrap().push(body.clone());
            Ok("txhash".into())
        }
        fn run_get_int(&self, _addr: &WalletAddress, _method: &str) -> Result<i128, SettleError> {
            Ok(0)
        }
    }

    #[test]
    fn ton_settlement_builds_and_sends_settle() {
        let s = TonSettlement::new(RecordingRpc::default());
        let h = EscrowHandle {
            job: JobId("j".into()),
            address: WalletAddress::new(0, [1u8; 32]),
            max_bid: 100,
        };
        let outcome = SettlementOutcome {
            result_hash: [4u8; 32],
            base: 60,
            winner: crate::types::Payout {
                to: WalletAddress::new(0, [2u8; 32]),
                amount: 60,
            },
            participants: vec![],
            platform_fee: 2,
        };
        assert!(s.settle(&h, &outcome).is_ok());
    }

    /// Transport whose `await_confirmation` returns a configurable outcome, so we
    /// can prove the on-chain confirmation is wired into each send path and that a
    /// failed / unconfirmed transaction surfaces as a typed error instead of being
    /// silently treated as success (the fire-and-forget bug this fixes).
    #[derive(Clone, Copy)]
    enum Confirm {
        Ok,
        Failed(i32),
        Unconfirmed,
    }
    struct ConfirmingRpc {
        confirm: Confirm,
        sends: Mutex<u32>,
    }
    impl ConfirmingRpc {
        fn new(confirm: Confirm) -> Self {
            Self {
                confirm,
                sends: Mutex::new(0),
            }
        }
    }
    impl TonRpc for ConfirmingRpc {
        fn send_internal(
            &self,
            _to: &WalletAddress,
            _amount: Amount,
            _body: &MessageBody,
        ) -> Result<String, SettleError> {
            *self.sends.lock().unwrap() += 1;
            Ok("tx".into())
        }
        fn run_get_int(&self, _addr: &WalletAddress, _method: &str) -> Result<i128, SettleError> {
            Ok(0)
        }
        fn await_confirmation(
            &self,
            _account: &WalletAddress,
            _after_lt: u64,
            _deadline_secs: u64,
        ) -> Result<(), SettleError> {
            match self.confirm {
                Confirm::Ok => Ok(()),
                Confirm::Failed(c) => Err(SettleError::TxFailed { exit_code: c }),
                Confirm::Unconfirmed => Err(SettleError::TxUnconfirmed),
            }
        }
    }

    fn sample_settle() -> (EscrowHandle, SettlementOutcome) {
        (
            EscrowHandle {
                job: JobId("j".into()),
                address: WalletAddress::new(0, [1u8; 32]),
                max_bid: 100,
            },
            SettlementOutcome {
                result_hash: [4u8; 32],
                base: 60,
                winner: crate::types::Payout {
                    to: WalletAddress::new(0, [2u8; 32]),
                    amount: 60,
                },
                participants: vec![],
                platform_fee: 2,
            },
        )
    }

    #[test]
    fn settle_ok_when_confirmed() {
        let s = TonSettlement::new(ConfirmingRpc::new(Confirm::Ok));
        let (h, o) = sample_settle();
        assert!(s.settle(&h, &o).is_ok());
    }

    #[test]
    fn settle_surfaces_onchain_failure_instead_of_silent_success() {
        let s = TonSettlement::new(ConfirmingRpc::new(Confirm::Failed(47)));
        let (h, o) = sample_settle();
        assert!(
            matches!(
                s.settle(&h, &o),
                Err(SettleError::TxFailed { exit_code: 47 })
            ),
            "a destination tx that threw must surface as TxFailed, not Ok"
        );
    }

    #[test]
    fn settle_surfaces_unconfirmed() {
        let s = TonSettlement::new(ConfirmingRpc::new(Confirm::Unconfirmed));
        let (h, o) = sample_settle();
        assert!(matches!(s.settle(&h, &o), Err(SettleError::TxUnconfirmed)));
    }

    #[test]
    fn refund_requires_confirmation() {
        let s = TonSettlement::new(ConfirmingRpc::new(Confirm::Unconfirmed));
        let h = sample_settle().0;
        assert!(matches!(s.refund(&h), Err(SettleError::TxUnconfirmed)));
    }

    #[test]
    fn submit_root_does_not_advance_prev_on_unconfirmed() {
        // A submit whose confirmation fails must NOT advance the locally tracked
        // prev-root (else a retry would chain onto a root that never anchored).
        let anchor = TonRecordAnchor::new(
            ConfirmingRpc::new(Confirm::Unconfirmed),
            WalletAddress::new(0, [9u8; 32]),
        );
        anchor.append(&JobRecord {
            job_id: "job-1".into(),
            query_hash: "q".into(),
            requester_wallet: "0:1111111111111111111111111111111111111111111111111111111111111111"
                .into(),
            max_bid: 1_000,
            result_hash: "r".into(),
            epoch: 1,
            prev_root: [0u8; 32],
            params_version: 0,
            input_fingerprint: String::new(),
        });
        let root_before = anchor.epoch_root();
        assert!(matches!(
            anchor.submit_root(1, 1_000),
            Err(SettleError::TxUnconfirmed)
        ));
        // epoch_root is derived from the (unchanged) leaf set; the failed submit
        // must not have mutated tracked state in a way that changes it.
        assert_eq!(anchor.epoch_root(), root_before);
    }

    /// RPC fake that answers `run_get_int` from a fixed method→value table, so
    /// the `GlobalParamsClient` read path is exercised with no network.
    #[derive(Default)]
    struct GetMethodRpc {
        values: HashMap<String, i128>,
        addresses: HashMap<String, WalletAddress>,
    }
    impl TonRpc for GetMethodRpc {
        fn send_internal(
            &self,
            _to: &WalletAddress,
            _amount: Amount,
            _body: &MessageBody,
        ) -> Result<String, SettleError> {
            Ok("ok".into())
        }
        fn run_get_int(&self, _addr: &WalletAddress, method: &str) -> Result<i128, SettleError> {
            self.values
                .get(method)
                .copied()
                .ok_or_else(|| SettleError::Backend(format!("no value for {method}")))
        }
        fn run_get_address(
            &self,
            _addr: &WalletAddress,
            method: &str,
        ) -> Result<Option<WalletAddress>, SettleError> {
            Ok(self.addresses.get(method).copied())
        }
    }

    #[test]
    fn global_params_client_reads_version_and_policy() {
        let mut values = HashMap::new();
        values.insert("get_params_version".to_string(), 7i128);
        values.insert("get_platform_fee_bps".to_string(), 250);
        values.insert("get_participation_commission_bps".to_string(), 150);
        values.insert("get_slash_failed_commitment_bps".to_string(), 2000);
        values.insert("get_attempt_deadline_ms".to_string(), 90_000);
        values.insert("get_progress_interval_ms".to_string(), 3_000);
        values.insert("get_progress_stall_mult".to_string(), 4);
        let mut addresses = HashMap::new();
        let treasury = WalletAddress::new(0, [0x7E; 32]);
        addresses.insert("get_fee_recipient".to_string(), treasury);
        let client = GlobalParamsClient::new(
            GetMethodRpc { values, addresses },
            WalletAddress::new(0, [0xAB; 32]),
        );

        assert_eq!(client.params_version().unwrap(), 7);
        let p = client.read_policy().unwrap();
        assert_eq!(p.version, 7);
        assert_eq!(p.platform_fee_bps, 250);
        // The authoritative fee recipient is read from chain (get_fee_recipient).
        assert_eq!(p.fee_recipient, treasury);
        assert_eq!(p.slash_failed_commitment_bps, 2000);
        assert_eq!(p.attempt_deadline_ms, 90_000);
        assert_eq!(p.progress_stall_mult, 4);

        // Overlay onto config as the authoritative layer for paid jobs.
        let mut econ = p2p_config::EconomicsConfig::default();
        let mut sched = p2p_config::SchedulerConfig::default();
        let applied = client.sync_into(&mut econ, &mut sched).unwrap();
        assert_eq!(applied.version, 7);
        assert!((econ.fees.platform_fee_pct - 0.025).abs() < 1e-9);
        assert!((econ.fees.participation_commission_frac - 0.015).abs() < 1e-9);
        assert!((econ.slashing.slash_failed_commitment_pct - 0.2).abs() < 1e-9);
        assert_eq!(sched.attempt_deadline_ms, 90_000);
        assert_eq!(sched.progress_interval_ms, 3_000);
        assert_eq!(sched.progress_stall_multiplier, 4);
    }

    #[test]
    fn open_with_terms_rejects_local_treasury_disagreeing_with_chain() {
        // FEE-RECIPIENT enforcement: a locally-configured treasury that disagrees
        // with the authoritative on-chain `GlobalParams.fee_recipient` is REJECTED
        // before any deploy — the admin treasury cannot be silently overridden.
        let code = CellBuilder::new().store_uint(0xC0DE, 16).build();
        let chain = WalletAddress::new(0, [0x7E; 32]);
        let wrong = WalletAddress::new(0, [0x11; 32]);
        let s = TonSettlement::with_escrow_code(
            RecordingRpc::default(),
            code,
            Cell::default(),
            WalletAddress::new(0, [1; 32]),
        )
        .with_requester(WalletAddress::new(0, [1; 32]))
        .with_treasury(wrong);
        let err = s
            .open_escrow_with_terms(
                &JobId("j".into()),
                100,
                &[3u8; 32],
                7,
                &[],
                Some(chain),
                None,
                None,
            )
            .unwrap_err();
        match err {
            SettleError::TreasuryMismatch {
                bound,
                expected,
                params_version,
            } => {
                assert_eq!(bound, wrong.to_raw_string());
                assert_eq!(expected, chain.to_raw_string());
                assert_eq!(params_version, 7);
            }
            other => panic!("expected TreasuryMismatch, got {other:?}"),
        }
    }

    #[test]
    fn open_with_terms_binds_chain_recipient_when_local_matches_or_is_unset() {
        // When the local treasury MATCHES the chain value (or is unset), the
        // treasury check passes and the chain value is bound — the open then fails
        // later at the (unsupported) deploy, NOT with a TreasuryMismatch. This pins
        // that a matching/absent local override is accepted, chain value wins.
        let chain = WalletAddress::new(0, [0x7E; 32]);
        let mk = || {
            TonSettlement::with_escrow_code(
                RecordingRpc::default(),
                CellBuilder::new().store_uint(0xC0DE, 16).build(),
                Cell::default(),
                WalletAddress::new(0, [1; 32]),
            )
            .with_requester(WalletAddress::new(0, [1; 32]))
        };
        let e1 = mk()
            .with_treasury(chain)
            .open_escrow_with_terms(
                &JobId("j".into()),
                100,
                &[3u8; 32],
                7,
                &[],
                Some(chain),
                None,
                None,
            )
            .unwrap_err();
        assert!(
            !matches!(e1, SettleError::TreasuryMismatch { .. }),
            "a local treasury equal to the chain value must NOT be a mismatch, got {e1:?}"
        );
        let e2 = mk()
            .open_escrow_with_terms(
                &JobId("j".into()),
                100,
                &[3u8; 32],
                7,
                &[],
                Some(chain),
                None,
                None,
            )
            .unwrap_err();
        assert!(
            !matches!(e2, SettleError::TreasuryMismatch { .. }),
            "no local treasury ⇒ chain value bound, not a mismatch, got {e2:?}"
        );
    }

    #[test]
    fn null_rpc_reports_disabled() {
        let r = NullTonRpc;
        assert!(r
            .send_internal(
                &WalletAddress::new(0, [0u8; 32]),
                0,
                &build_stake_deposit(0, 0)
            )
            .is_err());
        // deploy is unsupported on the default transport too.
        assert!(r
            .deploy(
                &WalletAddress::new(0, [0u8; 32]),
                0,
                &Cell::default(),
                &build_escrow_topup(0)
            )
            .is_err());
    }

    #[test]
    fn body_cells_carry_opcode_and_match_flat_abi_prefix() {
        // The live TL-B `cell` and the flat test `bytes` must agree on the opcode
        // (first 32 bits) for every op we broadcast.
        for mb in [
            build_stake_deposit(1, 100),
            build_stake_slash(2, 50, SlashReason::Cheat, &WalletAddress::new(0, [7u8; 32])),
            build_stake_unbond(3, 10),
            build_escrow_settle(
                4,
                &[3u8; 32],
                &WalletAddress::new(0, [2u8; 32]),
                60,
                60,
                2,
                &[],
                &[WalletAddress::new(0, [2u8; 32])],
            ),
            build_escrow_refund(5),
            build_escrow_topup(6),
            build_anchor_submit(7, 9, &[1u8; 32], &[0u8; 32], 1000),
            build_update_admin(8, &WalletAddress::new(0, [1u8; 32])),
        ] {
            // Cell's first 32 data bits == opcode == flat bytes[0..4].
            let top = &mb.cell.repr_hash(); // forces a valid (built) cell
            assert_eq!(top.len(), 32);
            assert_eq!(&mb.bytes[0..4], &mb.opcode.to_be_bytes());
            assert!(mb.cell.bit_len() >= 32);
        }
        // An EscrowSettle with empty participants AND empty candidates appends two
        // empty HashmapE `0` bits, so no extra ref.
        let settle = build_escrow_settle(
            0,
            &[0u8; 32],
            &WalletAddress::new(0, [0u8; 32]),
            1,
            1,
            1,
            &[],
            &[],
        );
        assert!(settle.cell.refs().is_empty());
        // A non-empty participants dict AND a non-empty candidates dict each add a
        // `1` bit + a dictionary root ref ⇒ two refs.
        let settle_p = build_escrow_settle(
            0,
            &[0u8; 32],
            &WalletAddress::new(0, [0u8; 32]),
            1,
            1,
            1,
            &[Payout {
                to: WalletAddress::new(0, [9u8; 32]),
                amount: 3,
            }],
            &[WalletAddress::new(0, [9u8; 32])],
        );
        assert_eq!(
            settle_p.cell.refs().len(),
            2,
            "participants + candidates each live in a ^dict root"
        );
        // UpdateParams's live cell puts EcoParams in a child ref (flat ABI inlines).
        let p = sample_params();
        let up = build_update_params(0, &WalletAddress::new(0, [0u8; 32]), &p);
        assert_eq!(up.cell.refs().len(), 1, "EcoParams lives in a ^child cell");
    }

    /// A recording RPC that, for every op, ALSO assembles a signed wallet-v5r1
    /// external message from the body cell (fixed test wallet + seqno) and
    /// asserts it is broadcastable (BoC parses, signature verifies). This ties the
    /// `ton` impls' payloads to the real signer without any network.
    struct SigningRecorder {
        wallet: crate::wallet::WalletV5R1,
        key: crate::wallet::WalletKey,
        sent: Mutex<Vec<(WalletAddress, Amount, u32, bool)>>, // to, amount, opcode-ish, deploy?
    }
    impl SigningRecorder {
        fn new() -> Self {
            let key = crate::wallet::WalletKey::from_seed(&[42u8; 32]);
            let wallet = crate::wallet::WalletV5R1::testnet(key.public_key());
            Self {
                wallet,
                key,
                sent: Mutex::new(Vec::new()),
            }
        }
        fn sign_and_check(
            &self,
            to: &WalletAddress,
            amount: Amount,
            body: &MessageBody,
            init: Option<Cell>,
        ) {
            let msg = crate::wallet::InternalMessage {
                dest: *to,
                value: amount,
                body: body.cell.clone(),
                state_init: init.clone(),
                mode: 3,
            };
            let boc = crate::wallet::build_signed_external_v5r1(
                &self.wallet,
                &self.key,
                11,
                2_000_000_000,
                &[msg],
            )
            .expect("payload signs");
            assert!(Cell::from_boc(&boc).is_some(), "signed external BoC parses");
            self.sent
                .lock()
                .unwrap()
                .push((*to, amount, body.opcode, init.is_some()));
        }
    }
    impl TonRpc for SigningRecorder {
        fn send_internal(
            &self,
            to: &WalletAddress,
            amount: Amount,
            body: &MessageBody,
        ) -> Result<String, SettleError> {
            self.sign_and_check(to, amount, body, None);
            Ok("ok".into())
        }
        fn run_get_int(&self, _addr: &WalletAddress, _method: &str) -> Result<i128, SettleError> {
            Ok(0)
        }
        fn deploy(
            &self,
            to: &WalletAddress,
            amount: Amount,
            state_init: &Cell,
            body: &MessageBody,
        ) -> Result<String, SettleError> {
            self.sign_and_check(to, amount, body, Some(state_init.clone()));
            Ok("ok".into())
        }
    }

    #[test]
    fn ton_ops_produce_signable_broadcast_payloads() {
        use std::sync::Arc;
        let rec = Arc::new(SigningRecorder::new());

        // --- settle: sends EscrowSettle to the escrow address, bounded by B. ---
        let s = TonSettlement::new(SigningRecorderRef(rec.clone()));
        let h = EscrowHandle {
            job: JobId("j".into()),
            address: WalletAddress::new(0, [1u8; 32]),
            max_bid: 100,
        };
        let outcome = SettlementOutcome {
            result_hash: [4u8; 32],
            base: 60,
            winner: crate::types::Payout {
                to: WalletAddress::new(0, [2u8; 32]),
                amount: 60,
            },
            participants: vec![],
            platform_fee: 2,
        };
        s.settle(&h, &outcome).unwrap();
        s.refund(&h).unwrap();

        // --- slash + unbond: send to the node's derived vault address. ---
        let code = CellBuilder::new().store_uint(0xE5C0, 16).build();
        let config = CellBuilder::new().store_uint(0xCF, 8).build();
        let node = NodeId("b3:n".into());
        let mut inits = HashMap::new();
        inits.insert(
            node.clone(),
            VaultInit {
                owner: WalletAddress::new(0, [5u8; 32]),
                binding_hash: [9u8; 32],
            },
        );
        let reg =
            TonStakeRegistry::new(SigningRecorderRef(rec.clone()), 0, 100, code, config, inits);
        reg.slash(&node, SlashReason::WrongResult, 10).unwrap();
        reg.request_unbond(&node, 5).unwrap();

        // --- open_escrow: deploys (state_init present) funded with the max bid. ---
        let escrow_code = CellBuilder::new().store_uint(0xE5C1, 16).build();
        let terms = CellBuilder::new().store_uint(0x7e, 8).build();
        let settlement = TonSettlement::with_escrow_code(
            SigningRecorderRef(rec.clone()),
            escrow_code,
            terms,
            WalletAddress::new(0, [0xAB; 32]),
        )
        .with_requester(WalletAddress::new(0, [0x11; 32]));
        let handle = settlement.open_escrow(&JobId("k".into()), 1_000).unwrap();
        assert_eq!(handle.max_bid, 1_000);

        // --- anchor submit: broadcasts the epoch root. ---
        let anchor = TonRecordAnchor::new(
            SigningRecorderRef(rec.clone()),
            WalletAddress::new(0, [0xDD; 32]),
        );
        anchor.append(&JobRecord {
            job_id: "k".into(),
            query_hash: "q".into(),
            requester_wallet: "0:00".into(),
            max_bid: 1,
            result_hash: "r".into(),
            epoch: 1,
            prev_root: [0u8; 32],
            params_version: 0,
            input_fingerprint: String::new(),
        });
        anchor.submit_root(1, 500).unwrap();

        let sent = rec.sent.lock().unwrap();
        // settle, refund, slash, unbond, open_escrow(deploy), anchor = 6 ops.
        assert_eq!(sent.len(), 6);
        // The open_escrow op is the only deploy, funded with the max bid 1000.
        assert!(sent
            .iter()
            .any(|(_, amt, op, dep)| *dep && *amt == 1_000 && *op == OP_ESCROW_TOPUP));
        // settle went out as EscrowSettle to the escrow address.
        assert!(sent
            .iter()
            .any(|(to, _, op, _)| *op == OP_ESCROW_SETTLE && to.hash == [1u8; 32]));
        assert!(sent.iter().any(|(_, _, op, _)| *op == OP_STAKE_SLASH));
        assert!(sent.iter().any(|(_, _, op, _)| *op == OP_ANCHOR_SUBMIT));
    }

    /// Newtype so an `Arc<SigningRecorder>` satisfies the `TonRpc` bound the ton
    /// impls require by value.
    struct SigningRecorderRef(std::sync::Arc<SigningRecorder>);
    impl TonRpc for SigningRecorderRef {
        fn send_internal(
            &self,
            to: &WalletAddress,
            amount: Amount,
            body: &MessageBody,
        ) -> Result<String, SettleError> {
            self.0.send_internal(to, amount, body)
        }
        fn run_get_int(&self, addr: &WalletAddress, method: &str) -> Result<i128, SettleError> {
            self.0.run_get_int(addr, method)
        }
        fn deploy(
            &self,
            to: &WalletAddress,
            amount: Amount,
            state_init: &Cell,
            body: &MessageBody,
        ) -> Result<String, SettleError> {
            self.0.deploy(to, amount, state_init, body)
        }
    }

    // -- StateInit address derivation, pinned to the Acton emulator -----------
    // Reference hashes come from `ton/scripts/_probe_addr.tolk`, which builds the
    // identical code/data cells on-chain and prints `StateInit.calcHashCodeData`.
    // If the off-chain cell encoding drifts from TON's, these break.

    fn hex32(s: &str) -> Hash32 {
        let mut h = [0u8; 32];
        h.copy_from_slice(&hex::decode(s).unwrap());
        h
    }

    #[test]
    fn state_init_address_matches_onchain_calc_hash_code_data() {
        let code = CellBuilder::new().store_uint(0xC0DE, 16).build();
        let data = CellBuilder::new().store_uint(0x1234_5678, 32).build();
        let addr = WalletAddress::from_state_init(BASECHAIN, &code, &data);
        assert_eq!(addr.workchain, 0);
        assert_eq!(
            hex::encode(addr.hash),
            "17a0e699e194a1aa4227c1d0a7057f193a45fb892da5c810f0a5ad4d571bab03"
        );
    }

    #[test]
    fn vault_data_state_init_matches_onchain() {
        // Same owner/binding/config the probe uses.
        let owner = WalletAddress::new(
            0,
            hex32("00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"),
        );
        // probe BINDING_HASH = 0xdeadbeef repeated 8 times (32 bytes).
        let binding = hex32("deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef");
        let code = CellBuilder::new().store_uint(0xC0DE, 16).build();
        let config = CellBuilder::new().store_uint(0xCF, 8).build();
        let init = VaultInit {
            owner,
            binding_hash: binding,
        };
        let data = build_vault_data(&init, config.clone());
        let addr = WalletAddress::from_state_init(BASECHAIN, &code, &data);
        // Pinned to the Acton emulator probe (ton/scripts/_probe_v2.tolk,
        // VAULT_SI_V2): the v2 `VaultStorage` includes the `upgrade` child ref
        // (codeVersion/pendingCodeHash/pendingCodeAt = 0) AND the trailing `pending`
        // empty `map<address, coins>` (`0` bit). The off-chain `build_vault_data`
        // must hash byte-identically to on-chain — this address moved when `pending`
        // was appended (additive storage change ⇒ new deterministic address).
        assert_eq!(
            hex::encode(addr.hash),
            "6f473bbc159ca24a8508fd881ee6050d17eaecd9ec31d5e50d4b959dbd9f0e30"
        );

        // Same registry resolves the node to that exact vault address.
        let mut inits = HashMap::new();
        let node = NodeId("b3:node-1".into());
        inits.insert(node.clone(), init);
        let reg = TonStakeRegistry::new(RecordingRpc::default(), 0, 100, code, config, inits);
        assert_eq!(reg.vault_of(&node), Some(addr));
        // An unbound node has no vault (no placeholder address).
        assert_eq!(reg.vault_of(&NodeId("b3:unknown".into())), None);
    }

    #[test]
    fn vault_config_cell_builds_byte_stably() {
        // Regression guard on the `VaultConfig` cell layout (field order / bit
        // sizes) via the deterministic vault address it produces. The pin catches
        // any accidental drift from `stake_types.tolk`. (Contract-PARITY against a
        // deployed vault additionally needs the real StakeVault/StakeReceiptWallet
        // code BoCs + an Acton probe — see `build_vault_config`'s wiring note.)
        let addr = WalletAddress::new(
            0,
            hex32("00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"),
        );
        let receipt = CellBuilder::new().store_uint(0xCAFE, 16).build();
        let cfg = build_vault_config(&VaultConfigParams {
            min_stake: 0,
            unbonding_period: 604_800,
            challenge_window: 86_400,
            keeper_grace: 86_400,
            keeper_bounty_bps: 200,
            slasher: addr.clone(),
            upgrade_authority: addr.clone(),
            receipt_wallet_code: receipt,
            slash: SlashSplitConfig {
                treasury: addr.clone(),
                redundancy_pool: addr.clone(),
                split_challenger_bps: 4000,
                split_redundancy_bps: 3000,
                split_burn_bps: 2000,
                split_treasury_bps: 1000,
            },
        });
        let code = CellBuilder::new().store_uint(0xC0DE, 16).build();
        let init = VaultInit {
            owner: addr,
            binding_hash: [0u8; 32],
        };
        let data = build_vault_data(&init, cfg);
        let derived = WalletAddress::from_state_init(BASECHAIN, &code, &data);
        assert_eq!(
            hex::encode(derived.hash),
            "25d5e3ac5c4d653d8960ce4bb714d840fadf12f21ac3c56d9469779146c21b0f"
        );
    }

    #[test]
    fn escrow_address_is_deterministic_and_stable() {
        let escrow_code = CellBuilder::new().store_uint(0xE5C0, 16).build();
        let terms = CellBuilder::new().store_uint(0x7e, 8).build();
        let arbiter = WalletAddress::new(0, [0xAB; 32]);
        let s =
            TonSettlement::with_escrow_code(RecordingRpc::default(), escrow_code, terms, arbiter);
        let requester = WalletAddress::new(0, [0x11; 32]);
        let a1 = s.escrow_address(&requester, 1_000, 42).unwrap();
        let a2 = s.escrow_address(&requester, 1_000, 42).unwrap();
        assert_eq!(a1, a2, "deterministic for identical init");
        // Different terms (max_bid) => different escrow contract address.
        let a3 = s.escrow_address(&requester, 2_000, 42).unwrap();
        assert_ne!(a1, a3);
        // Without wired code, derivation is unavailable (no placeholder).
        let s0 = TonSettlement::new(RecordingRpc::default());
        assert_eq!(s0.escrow_address(&requester, 1_000, 42), None);
    }
}
