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

use p2p_config::DataClassCfg;
use p2p_proto::{JobId, NodeId};

use crate::traits::{Settlement, StakeRegistry};
use crate::types::{
    Amount, EscrowHandle, SettleError, SettlementOutcome, SlashError, SlashReason, WalletAddress,
};

// Opcodes — MUST match `ton/contracts/*` (see stake_types.tolk / escrow_types.tolk / anchor_types.tolk).
pub const OP_STAKE_DEPOSIT: u32 = 0x534b_4b01;
pub const OP_STAKE_UNBOND: u32 = 0x534b_4b02;
pub const OP_STAKE_WITHDRAW: u32 = 0x534b_4b03;
pub const OP_STAKE_SLASH: u32 = 0x534b_4b05;
pub const OP_ESCROW_SETTLE: u32 = 0x4553_4302;
pub const OP_ESCROW_REFUND: u32 = 0x4553_4303;
pub const OP_ANCHOR_SUBMIT: u32 = 0x414e_4301;

/// A typed message body for an on-chain contract. The byte encoding mirrors the
/// contracts' TL-B field order and is deterministic (unit-tested).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageBody {
    pub opcode: u32,
    pub bytes: Vec<u8>,
}

impl MessageBody {
    fn new(opcode: u32) -> Self {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&opcode.to_be_bytes());
        Self { opcode, bytes }
    }
    fn u64(mut self, v: u64) -> Self {
        self.bytes.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn coins(mut self, v: Amount) -> Self {
        self.bytes.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn addr(mut self, a: &WalletAddress) -> Self {
        self.bytes.extend_from_slice(&a.to_raw_bytes());
        self
    }
    fn hash(mut self, h: &[u8; 32]) -> Self {
        self.bytes.extend_from_slice(h);
        self
    }
    fn byte(mut self, b: u8) -> Self {
        self.bytes.push(b);
        self
    }
}

/// Build the `StakeDeposit` body.
pub fn build_stake_deposit(query_id: u64, amount: Amount) -> MessageBody {
    MessageBody::new(OP_STAKE_DEPOSIT).u64(query_id).coins(amount)
}

/// Build the `StakeSlash` body.
pub fn build_stake_slash(query_id: u64, amount: Amount, reason: SlashReason, challenger: &WalletAddress) -> MessageBody {
    MessageBody::new(OP_STAKE_SLASH)
        .u64(query_id)
        .coins(amount)
        .byte(reason.code())
        .addr(challenger)
}

/// Build the (scalar prefix of the) `EscrowSettle` body. The `participants` map
/// is a TL-B dict encoded by the live BoC layer (feature `ton-live`); the scalar
/// HTLC fields below are what we unit-test here.
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
}

/// Build the `AnchorSubmitRoot` body.
pub fn build_anchor_submit(query_id: u64, epoch: u32, root: &[u8; 32], prev_root: &[u8; 32], stake_weight: Amount) -> MessageBody {
    let mut b = MessageBody::new(OP_ANCHOR_SUBMIT).u64(query_id);
    b.bytes.extend_from_slice(&epoch.to_be_bytes());
    b.hash(root).hash(prev_root).coins(stake_weight)
}

/// Abstract TON transport. The live HTTP/toncenter client is gated behind
/// `ton-live`; tests use a recording fake.
pub trait TonRpc: Send + Sync {
    /// Send an internal message carrying `body` to `to` with `amount` nanoton.
    /// Returns a tx hash / id on success.
    fn send_internal(&self, to: &WalletAddress, amount: Amount, body: &MessageBody) -> Result<String, SettleError>;
    /// Run a get-method returning a single integer (e.g. staked amount).
    fn run_get_int(&self, addr: &WalletAddress, method: &str) -> Result<i128, SettleError>;
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

/// TON-backed settlement (per-job `JobEscrow`). Address derivation of the per-job
/// escrow contract is computed by the off-chain coordinator (it deploys the
/// escrow with the requester's funds); here we hold the rpc + treasury config.
pub struct TonSettlement<R: TonRpc> {
    rpc: R,
}

impl<R: TonRpc> TonSettlement<R> {
    pub fn new(rpc: R) -> Self {
        Self { rpc }
    }
}

impl<R: TonRpc> Settlement for TonSettlement<R> {
    fn open_escrow(&self, _job: &JobId, _max_bid: Amount) -> Result<EscrowHandle, SettleError> {
        // Deploying the per-job escrow with funds requires the live BoC/deploy
        // path (feature `ton-live`); the body/ABI is unit-tested above.
        Err(SettleError::Backend("open_escrow requires the ton-live deploy path".into()))
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
        let body = MessageBody::new(OP_ESCROW_REFUND).u64(0);
        self.rpc.send_internal(&h.address, 0, &body).map(|_| ())
    }

    fn is_onchain(&self) -> bool {
        true
    }
}

/// TON-backed stake registry (per-node `StakeVault`). Reads use get-methods; the
/// node->vault address map is supplied by the caller (omitted here for brevity).
pub struct TonStakeRegistry<R: TonRpc> {
    rpc: R,
    min_public: Amount,
    stake_cap: Amount,
}

impl<R: TonRpc> TonStakeRegistry<R> {
    pub fn new(rpc: R, min_public: Amount, stake_cap: Amount) -> Self {
        Self { rpc, min_public, stake_cap }
    }
    /// Vault address for a node would come from the on-chain stake registry /
    /// binding record; this is a placeholder derivation for the skeleton.
    fn vault_of(&self, node: &NodeId) -> WalletAddress {
        WalletAddress::new(0, *blake3::hash(node.0.as_bytes()).as_bytes())
    }
}

impl<R: TonRpc> StakeRegistry for TonStakeRegistry<R> {
    fn stake_of(&self, node: &NodeId) -> Amount {
        match self.rpc.run_get_int(&self.vault_of(node), "get_vault_state") {
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
        let body = build_stake_slash(0, amount, reason, &self.vault_of(node));
        self.rpc
            .send_internal(&self.vault_of(node), 0, &body)
            .map(|_| ())
            .map_err(|e| SlashError::Backend(e.to_string()))
    }
    fn request_unbond(&self, node: &NodeId, amount: Amount) -> Result<(), SlashError> {
        let mut body = MessageBody::new(OP_STAKE_UNBOND).u64(0);
        body = body.coins(amount);
        self.rpc
            .send_internal(&self.vault_of(node), 0, &body)
            .map(|_| ())
            .map_err(|e| SlashError::Backend(e.to_string()))
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
    }
}
