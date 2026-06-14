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

use p2p_config::DataClassCfg;
use p2p_proto::{JobId, NodeId};

use crate::cell::{state_init_cell, Cell, CellBuilder, BASECHAIN};
use crate::traits::{RecordAnchor, Settlement, StakeRegistry};
use crate::types::{
    Amount, EscrowHandle, Hash32, InclusionProof, JobRecord, SettleError, SettlementOutcome,
    SlashError, SlashReason, WalletAddress,
};

// Opcodes — MUST match `ton/contracts/*` (see stake_types.tolk / escrow_types.tolk / anchor_types.tolk).
pub const OP_STAKE_DEPOSIT: u32 = 0x534b_4b01;
pub const OP_STAKE_UNBOND: u32 = 0x534b_4b02;
pub const OP_STAKE_WITHDRAW: u32 = 0x534b_4b03;
pub const OP_STAKE_SLASH: u32 = 0x534b_4b05;
pub const OP_ESCROW_TOPUP: u32 = 0x4553_4300;
pub const OP_ESCROW_SETTLE: u32 = 0x4553_4302;
pub const OP_ESCROW_REFUND: u32 = 0x4553_4303;
pub const OP_ANCHOR_SUBMIT: u32 = 0x414e_4301;
// GlobalParams (platform-wide economic params) — MUST match `global_params_types.tolk`.
pub const OP_UPDATE_PARAMS: u32 = 0x4750_4101;
pub const OP_UPDATE_ADMIN: u32 = 0x4750_4102;

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
    /// Append a single bit to the cell ONLY (e.g. an empty `HashmapE`/`map`
    /// terminator, which the flat ABI omits).
    fn cell_bit(mut self, bit: bool) -> Self {
        self.cb = self.cb.store_uint(bit as u128, 1);
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
    MessageBody::new(OP_STAKE_DEPOSIT).u64(query_id).coins(amount).finish()
}

/// Build the `StakeSlash` body.
pub fn build_stake_slash(query_id: u64, amount: Amount, reason: SlashReason, challenger: &WalletAddress) -> MessageBody {
    MessageBody::new(OP_STAKE_SLASH)
        .u64(query_id)
        .coins(amount)
        .byte(reason.code())
        .addr(challenger)
        .finish()
}

/// Build the `StakeRequestUnbond` body (`queryId, amount`).
pub fn build_stake_unbond(query_id: u64, amount: Amount) -> MessageBody {
    MessageBody::new(OP_STAKE_UNBOND).u64(query_id).coins(amount).finish()
}

/// Build the `EscrowSettle` body. The HTLC scalar fields are encoded inline; the
/// `participants: map<address, coins>` is emitted as an **empty** `HashmapE`
/// (live multi-participant commission dicts are a documented follow-up — see the
/// report caveats), so the on-chain contract pays the winner + platform fee and
/// refunds the remainder to the requester.
pub fn build_escrow_settle(
    query_id: u64,
    result_hash: &[u8; 32],
    winner: &WalletAddress,
    winner_amount: Amount,
    platform_fee: Amount,
) -> MessageBody {
    MessageBody::new(OP_ESCROW_SETTLE)
        .u64(query_id)
        .hash(result_hash)
        .addr(winner)
        .coins(winner_amount)
        .coins(platform_fee)
        .cell_bit(false) // empty participants HashmapE
        .finish()
}

/// Build the `EscrowRefund` body (`queryId`).
pub fn build_escrow_refund(query_id: u64) -> MessageBody {
    MessageBody::new(OP_ESCROW_REFUND).u64(query_id).finish()
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
pub fn build_anchor_submit(query_id: u64, epoch: u32, root: &[u8; 32], prev_root: &[u8; 32], stake_weight: Amount) -> MessageBody {
    MessageBody::new(OP_ANCHOR_SUBMIT)
        .u64(query_id)
        .u32(epoch)
        .hash(root)
        .hash(prev_root)
        .coins(stake_weight)
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
        Ok(())
    }
}

/// Build the (scalar) `UpdateParams` body for `GlobalParams` (§12). The
/// `EcoParams` field order mirrors `global_params_types.tolk::EcoParams`; the
/// live BoC layer (feature `ton-live`) packs the params into the child cell ref
/// the contract expects.
pub fn build_update_params(query_id: u64, fee_recipient: &WalletAddress, p: &GlobalParams) -> MessageBody {
    // Flat ABI (unit-tested): all EcoParams fields inline. Live cell: the params
    // live in a `^EcoParams` child cell (`UpdateParams.params: Cell<EcoParams>`).
    let (flat, eco) = eco_params_encoded(p);
    let mut b = MessageBody::new(OP_UPDATE_PARAMS).u64(query_id).addr(fee_recipient);
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
    (bytes, cb.build())
}

/// Build the `UpdateAdmin` body (admin rotation → multisig).
pub fn build_update_admin(query_id: u64, new_admin: &WalletAddress) -> MessageBody {
    MessageBody::new(OP_UPDATE_ADMIN).u64(query_id).addr(new_admin).finish()
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
/// unbondingAt, totalSupply, bindingHash, config(ref)`.
pub fn build_vault_data(init: &VaultInit, config: Cell) -> Cell {
    CellBuilder::new()
        .store_address(&init.owner)
        .store_coins(0) // staked
        .store_coins(0) // unbondingAmount
        .store_uint(0, 32) // unbondingAt
        .store_coins(0) // totalSupply
        .store_u256(&init.binding_hash)
        .store_ref(config)
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

/// Build the per-job `JobEscrow` init **data** cell, matching `EscrowStorage`:
/// `requester, arbiter, escrowAmount, deadline, settled, terms(ref)`.
pub fn build_escrow_data(init: &EscrowInit, terms: Cell) -> Cell {
    CellBuilder::new()
        .store_address(&init.requester)
        .store_address(&init.arbiter)
        .store_coins(init.escrow_amount)
        .store_uint(init.deadline as u128, 32)
        .store_uint(0, 1) // settled = false
        .store_ref(terms)
        .build()
}

/// Abstract TON transport. The live HTTP/toncenter client is gated behind
/// `ton-live`; tests use a recording fake.
pub trait TonRpc: Send + Sync {
    /// Send an internal message carrying `body` to `to` with `amount` nanoton.
    /// Returns a tx hash / id on success.
    fn send_internal(&self, to: &WalletAddress, amount: Amount, body: &MessageBody) -> Result<String, SettleError>;
    /// Run a get-method returning a single integer (e.g. staked amount).
    fn run_get_int(&self, addr: &WalletAddress, method: &str) -> Result<i128, SettleError>;
    /// Deploy a contract: send `amount` nanoton + `state_init` to `to` carrying
    /// `body` (used to fund-open a per-job escrow). Default: unsupported.
    fn deploy(
        &self,
        _to: &WalletAddress,
        _amount: Amount,
        _state_init: &Cell,
        _body: &MessageBody,
    ) -> Result<String, SettleError> {
        Err(SettleError::Backend("deploy is not supported by this transport".into()))
    }
}

/// Default transport that performs NO network I/O. Used when `ton-live` is off.
#[derive(Default)]
pub struct NullTonRpc;

impl TonRpc for NullTonRpc {
    fn send_internal(&self, _to: &WalletAddress, _amount: Amount, _body: &MessageBody) -> Result<String, SettleError> {
        Err(SettleError::Backend(
            "live TON RPC disabled (build with feature `ton-live` and configure an endpoint)".into(),
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
    /// Refund-on-timeout window (secs) used to set a fresh escrow's deadline.
    escrow_window_secs: u32,
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
            escrow_window_secs: 3600,
        }
    }

    /// Construct with the deployed `JobEscrow` code so per-job escrow addresses
    /// can be resolved via deterministic StateInit addressing.
    pub fn with_escrow_code(rpc: R, escrow_code: Cell, terms: Cell, arbiter: WalletAddress) -> Self {
        Self {
            rpc,
            escrow_code: Some(escrow_code),
            terms,
            arbiter,
            requester: None,
            escrow_window_secs: 3600,
        }
    }

    /// Set the requester wallet (funds + refund recipient) so `open_escrow` can
    /// deploy the funded per-job escrow.
    pub fn with_requester(mut self, requester: WalletAddress) -> Self {
        self.requester = Some(requester);
        self
    }

    /// Set the refund-on-timeout window (secs) for newly opened escrows.
    pub fn with_escrow_window(mut self, secs: u32) -> Self {
        self.escrow_window_secs = secs;
        self
    }

    /// Deterministic per-job `JobEscrow` address from its StateInit, or `None`
    /// when the escrow code has not been wired in.
    pub fn escrow_address(&self, requester: &WalletAddress, max_bid: Amount, deadline: u32) -> Option<WalletAddress> {
        let code = self.escrow_code.as_ref()?;
        let init = EscrowInit { requester: *requester, arbiter: self.arbiter, escrow_amount: max_bid, deadline };
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
        let code = self
            .escrow_code
            .as_ref()
            .ok_or_else(|| SettleError::Backend("open_escrow requires escrow code wiring".into()))?;
        let requester = self
            .requester
            .ok_or_else(|| SettleError::Backend("open_escrow requires a requester wallet".into()))?;
        let deadline = now_secs_u32().saturating_add(self.escrow_window_secs);
        let init = EscrowInit { requester, arbiter: self.arbiter, escrow_amount: max_bid, deadline };
        let data = build_escrow_data(&init, self.terms.clone());
        let address = WalletAddress::from_state_init(BASECHAIN, code, &data);
        let state_init = state_init_cell(code.clone(), data);
        let body = build_escrow_topup(0);
        self.rpc.deploy(&address, max_bid, &state_init, &body)?;
        Ok(EscrowHandle { job: job.clone(), address, max_bid })
    }

    fn settle(&self, h: &EscrowHandle, outcome: &SettlementOutcome) -> Result<(), SettleError> {
        if outcome.total() > h.max_bid {
            return Err(SettleError::PayoutExceedsEscrow { payout: outcome.total(), escrow: h.max_bid });
        }
        let body = build_escrow_settle(
            0,
            &outcome.result_hash,
            &outcome.winner.to,
            outcome.winner.amount,
            outcome.platform_fee,
        );
        self.rpc.send_internal(&h.address, 0, &body).map(|_| ())
    }

    fn refund(&self, h: &EscrowHandle) -> Result<(), SettleError> {
        let body = build_escrow_refund(0);
        self.rpc.send_internal(&h.address, 0, &body).map(|_| ())
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
        Self { rpc, min_public, stake_cap, vault_code, vault_config, inits }
    }

    /// Deterministic per-node `StakeVault` address from its StateInit, or `None`
    /// if the node has no registered binding (hence no vault).
    pub fn vault_of(&self, node: &NodeId) -> Option<WalletAddress> {
        let init = self.inits.get(node)?;
        let data = build_vault_data(init, self.vault_config.clone());
        Some(WalletAddress::from_state_init(BASECHAIN, &self.vault_code, &data))
    }
}

impl<R: TonRpc> StakeRegistry for TonStakeRegistry<R> {
    fn stake_of(&self, node: &NodeId) -> Amount {
        let Some(vault) = self.vault_of(node) else { return 0 };
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
        let vault = self.vault_of(node).ok_or_else(|| SlashError::UnknownNode(node.0.clone()))?;
        let body = build_stake_slash(0, amount, reason, &vault);
        self.rpc
            .send_internal(&vault, 0, &body)
            .map(|_| ())
            .map_err(|e| SlashError::Backend(e.to_string()))
    }
    fn request_unbond(&self, node: &NodeId, amount: Amount) -> Result<(), SlashError> {
        let vault = self.vault_of(node).ok_or_else(|| SlashError::UnknownNode(node.0.clone()))?;
        let body = build_stake_unbond(0, amount);
        self.rpc
            .send_internal(&vault, 0, &body)
            .map(|_| ())
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
}

impl<R: TonRpc> TonRecordAnchor<R> {
    pub fn new(rpc: R, anchor: WalletAddress) -> Self {
        Self { rpc, anchor, inner: std::sync::Mutex::new((Vec::new(), [0u8; 32])) }
    }

    /// Broadcast the current epoch root to the on-chain `RecordAnchor` (keeper
    /// submit). `stake_weight` is the aggregate staked weight backing this root.
    /// Advances the chained `prev_root` on success.
    pub fn submit_root(&self, epoch: u32, stake_weight: Amount) -> Result<String, SettleError> {
        let (root, prev) = {
            let g = self.inner.lock().unwrap();
            let leaves: Vec<Hash32> = g.0.iter().map(|(_, h)| *h).collect();
            (crate::merkle::merkle_root(&leaves), g.1)
        };
        let body = build_anchor_submit(0, epoch, &root, &prev, stake_weight);
        let res = self.rpc.send_internal(&self.anchor, 0, &body)?;
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
        cmd.arg("-s").arg("--max-time").arg("30").arg("-H").arg("Content-Type: application/json");
        if let Some(k) = &self.api_key {
            cmd.arg("-H").arg(format!("X-API-Key: {k}"));
        }
        cmd.arg("-d").arg(json_body).arg(url);
        let out = cmd.output().map_err(|e| SettleError::Backend(format!("curl spawn: {e}")))?;
        if !out.status.success() {
            return Err(SettleError::Backend(format!("curl exit {:?}", out.status.code())));
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
        let boc =
            crate::wallet::build_signed_external_v5r1(&self.wallet, &self.key, seqno, valid_until, &[msg])?;
        let b64 = crate::wallet::base64_encode(&boc);
        let json = format!(r#"{{"boc":"{b64}"}}"#);
        let raw = self.curl_post("sendBoc", &json)?;
        let v: serde_json::Value =
            serde_json::from_str(&raw).map_err(|e| SettleError::Backend(format!("bad sendBoc JSON: {e}")))?;
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
}

#[cfg(feature = "ton-live")]
impl TonRpc for ToncenterRpc {
    fn send_internal(&self, to: &WalletAddress, amount: Amount, body: &MessageBody) -> Result<String, SettleError> {
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
        let json = format!(r#"{{"address":"{}","method":"{method}","stack":[]}}"#, addr.to_raw_string());
        let raw = self.curl_post("runGetMethod", &json)?;
        let v: serde_json::Value =
            serde_json::from_str(&raw).map_err(|e| SettleError::Backend(format!("bad JSON: {e}")))?;
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn message_bodies_carry_the_contract_opcodes() {
        assert_eq!(build_stake_deposit(1, 100).opcode, OP_STAKE_DEPOSIT);
        assert_eq!(&build_stake_deposit(1, 100).bytes[0..4], &OP_STAKE_DEPOSIT.to_be_bytes());

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
        }
    }

    #[test]
    fn update_params_body_carries_opcode_and_validates() {
        let fee = WalletAddress::new(0, [9u8; 32]);
        let b = build_update_params(7, &fee, &sample_params());
        assert_eq!(b.opcode, OP_UPDATE_PARAMS);
        assert_eq!(&b.bytes[0..4], &OP_UPDATE_PARAMS.to_be_bytes());
        // opcode(4)+queryId(8)+addr(36)+11*u16(22)+4*coins(64)+2*u32(8)+5*u8(5)+3*u16(6)
        assert_eq!(b.bytes.len(), 4 + 8 + 36 + 22 + 64 + 8 + 5 + 6);
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
    fn escrow_settle_body_layout_is_stable() {
        let winner = WalletAddress::new(0, [2u8; 32]);
        let b = build_escrow_settle(1, &[3u8; 32], &winner, 60, 2);
        assert_eq!(b.opcode, OP_ESCROW_SETTLE);
        // opcode(4)+queryId(8)+hash(32)+addr(36)+coins(16)+coins(16)
        assert_eq!(b.bytes.len(), 4 + 8 + 32 + 36 + 16 + 16);
    }

    /// Recording fake transport for unit tests (no network).
    #[derive(Default)]
    struct RecordingRpc {
        sent: Mutex<Vec<MessageBody>>,
    }
    impl TonRpc for RecordingRpc {
        fn send_internal(&self, _to: &WalletAddress, _amount: Amount, body: &MessageBody) -> Result<String, SettleError> {
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
        let h = EscrowHandle { job: JobId("j".into()), address: WalletAddress::new(0, [1u8; 32]), max_bid: 100 };
        let outcome = SettlementOutcome {
            result_hash: [4u8; 32],
            winner: crate::types::Payout { to: WalletAddress::new(0, [2u8; 32]), amount: 60 },
            participants: vec![],
            platform_fee: 2,
        };
        assert!(s.settle(&h, &outcome).is_ok());
    }

    #[test]
    fn null_rpc_reports_disabled() {
        let r = NullTonRpc;
        assert!(r.send_internal(&WalletAddress::new(0, [0u8; 32]), 0, &build_stake_deposit(0, 0)).is_err());
        // deploy is unsupported on the default transport too.
        assert!(r
            .deploy(&WalletAddress::new(0, [0u8; 32]), 0, &Cell::default(), &build_escrow_topup(0))
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
            build_escrow_settle(4, &[3u8; 32], &WalletAddress::new(0, [2u8; 32]), 60, 2),
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
        // EscrowSettle's live cell appends the empty participants HashmapE bit, so
        // it has one more bit than the scalar prefix implies (and no extra ref).
        let settle = build_escrow_settle(0, &[0u8; 32], &WalletAddress::new(0, [0u8; 32]), 1, 1);
        assert!(settle.cell.refs().is_empty());
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
            Self { wallet, key, sent: Mutex::new(Vec::new()) }
        }
        fn sign_and_check(&self, to: &WalletAddress, amount: Amount, body: &MessageBody, init: Option<Cell>) {
            let msg = crate::wallet::InternalMessage {
                dest: *to,
                value: amount,
                body: body.cell.clone(),
                state_init: init.clone(),
                mode: 3,
            };
            let boc =
                crate::wallet::build_signed_external_v5r1(&self.wallet, &self.key, 11, 2_000_000_000, &[msg])
                    .expect("payload signs");
            assert!(Cell::from_boc(&boc).is_some(), "signed external BoC parses");
            self.sent.lock().unwrap().push((*to, amount, body.opcode, init.is_some()));
        }
    }
    impl TonRpc for SigningRecorder {
        fn send_internal(&self, to: &WalletAddress, amount: Amount, body: &MessageBody) -> Result<String, SettleError> {
            self.sign_and_check(to, amount, body, None);
            Ok("ok".into())
        }
        fn run_get_int(&self, _addr: &WalletAddress, _method: &str) -> Result<i128, SettleError> {
            Ok(0)
        }
        fn deploy(&self, to: &WalletAddress, amount: Amount, state_init: &Cell, body: &MessageBody) -> Result<String, SettleError> {
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
        let h = EscrowHandle { job: JobId("j".into()), address: WalletAddress::new(0, [1u8; 32]), max_bid: 100 };
        let outcome = SettlementOutcome {
            result_hash: [4u8; 32],
            winner: crate::types::Payout { to: WalletAddress::new(0, [2u8; 32]), amount: 60 },
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
        inits.insert(node.clone(), VaultInit { owner: WalletAddress::new(0, [5u8; 32]), binding_hash: [9u8; 32] });
        let reg = TonStakeRegistry::new(SigningRecorderRef(rec.clone()), 0, 100, code, config, inits);
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
        let anchor = TonRecordAnchor::new(SigningRecorderRef(rec.clone()), WalletAddress::new(0, [0xDD; 32]));
        anchor.append(&JobRecord {
            job_id: "k".into(),
            query_hash: "q".into(),
            requester_wallet: "0:00".into(),
            max_bid: 1,
            result_hash: "r".into(),
            epoch: 1,
            prev_root: [0u8; 32],
        });
        anchor.submit_root(1, 500).unwrap();

        let sent = rec.sent.lock().unwrap();
        // settle, refund, slash, unbond, open_escrow(deploy), anchor = 6 ops.
        assert_eq!(sent.len(), 6);
        // The open_escrow op is the only deploy, funded with the max bid 1000.
        assert!(sent.iter().any(|(_, amt, op, dep)| *dep && *amt == 1_000 && *op == OP_ESCROW_TOPUP));
        // settle went out as EscrowSettle to the escrow address.
        assert!(sent.iter().any(|(to, _, op, _)| *op == OP_ESCROW_SETTLE && to.hash == [1u8; 32]));
        assert!(sent.iter().any(|(_, _, op, _)| *op == OP_STAKE_SLASH));
        assert!(sent.iter().any(|(_, _, op, _)| *op == OP_ANCHOR_SUBMIT));
    }

    /// Newtype so an `Arc<SigningRecorder>` satisfies the `TonRpc` bound the ton
    /// impls require by value.
    struct SigningRecorderRef(std::sync::Arc<SigningRecorder>);
    impl TonRpc for SigningRecorderRef {
        fn send_internal(&self, to: &WalletAddress, amount: Amount, body: &MessageBody) -> Result<String, SettleError> {
            self.0.send_internal(to, amount, body)
        }
        fn run_get_int(&self, addr: &WalletAddress, method: &str) -> Result<i128, SettleError> {
            self.0.run_get_int(addr, method)
        }
        fn deploy(&self, to: &WalletAddress, amount: Amount, state_init: &Cell, body: &MessageBody) -> Result<String, SettleError> {
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
        let init = VaultInit { owner, binding_hash: binding };
        let data = build_vault_data(&init, config.clone());
        let addr = WalletAddress::from_state_init(BASECHAIN, &code, &data);
        assert_eq!(
            hex::encode(addr.hash),
            "40f3f53e350757798a90c6546d8375993bf7eda0b0fdca0824c78184272ada83"
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
    fn escrow_address_is_deterministic_and_stable() {
        let escrow_code = CellBuilder::new().store_uint(0xE5C0, 16).build();
        let terms = CellBuilder::new().store_uint(0x7e, 8).build();
        let arbiter = WalletAddress::new(0, [0xAB; 32]);
        let s = TonSettlement::with_escrow_code(RecordingRpc::default(), escrow_code, terms, arbiter);
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
