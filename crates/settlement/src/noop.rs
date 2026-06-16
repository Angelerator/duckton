//! Genuine no-op implementations — the FREE, no-chain path.
//!
//! BLOCKCHAIN_ECONOMICS §8.2: a free job opens NO escrow, engages NO
//! stake/settlement/anchor, pays NO fees, and NEVER reaches a TON client. These
//! impls do exactly nothing (and hold no client handle), so a free job has zero
//! chain dependency and runs even if no chain/wallet is configured. Scoring
//! (reputation/quality) is independent and still runs in the coordinator.

use p2p_config::DataClassCfg;
use p2p_proto::{JobId, NodeId};

use crate::traits::{RecordAnchor, Settlement, StakeRegistry, Wallet};
use crate::types::{
    Amount, EscrowHandle, Hash32, InclusionProof, JobRecord, NodeWalletBinding, SettleError,
    SettlementOutcome, SlashError, SlashReason, WalletAddress,
};

/// A wallet placeholder for nodes that do no paid work (no wallet required).
#[derive(Default)]
pub struct NoopWallet;

impl Wallet for NoopWallet {
    fn address(&self) -> WalletAddress {
        WalletAddress::new(0, [0u8; 32])
    }
    fn verify_binding(&self, _b: &NodeWalletBinding, _now: u64) -> bool {
        // Free nodes need no wallet binding; nothing to verify.
        false
    }
}

/// Settlement that never settles. All methods are inert and never touch a client.
#[derive(Default)]
pub struct NoopSettlement;

impl Settlement for NoopSettlement {
    fn open_escrow(&self, job: &JobId, max_bid: Amount) -> Result<EscrowHandle, SettleError> {
        // No funds are locked; return an inert handle for type-compatibility.
        Ok(EscrowHandle {
            job: job.clone(),
            address: WalletAddress::new(0, [0u8; 32]),
            max_bid,
        })
    }
    fn settle(&self, _h: &EscrowHandle, _outcome: &SettlementOutcome) -> Result<(), SettleError> {
        Ok(())
    }
    fn refund(&self, _h: &EscrowHandle) -> Result<(), SettleError> {
        Ok(())
    }
    fn is_onchain(&self) -> bool {
        false
    }
}

/// Stake registry that knows of no stake — the free tier. Every node is eligible
/// for free/public work and gets zero stake-ranking boost.
#[derive(Default)]
pub struct NoopStakeRegistry;

impl StakeRegistry for NoopStakeRegistry {
    fn stake_of(&self, _node: &NodeId) -> Amount {
        0
    }
    fn is_eligible(&self, _node: &NodeId, class: DataClassCfg) -> bool {
        // Free path: only public/free work is served without stake.
        matches!(class, DataClassCfg::Public)
    }
    fn stake_factor(&self, _node: &NodeId) -> f64 {
        0.0
    }
    fn slash(
        &self,
        _node: &NodeId,
        _reason: SlashReason,
        _amount: Amount,
    ) -> Result<(), SlashError> {
        Ok(())
    }
    fn request_unbond(&self, _node: &NodeId, _amount: Amount) -> Result<(), SlashError> {
        Ok(())
    }
}

/// Record anchor that anchors nothing (free jobs keep receipts purely off-chain).
#[derive(Default)]
pub struct NoopRecordAnchor;

impl RecordAnchor for NoopRecordAnchor {
    fn append(&self, _record: &JobRecord) {}
    fn epoch_root(&self) -> Hash32 {
        [0u8; 32]
    }
    fn prove_inclusion(&self, _job: &JobId) -> Option<InclusionProof> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_settlement_is_inert_and_offchain() {
        let s = NoopSettlement;
        assert!(!s.is_onchain());
        let h = s.open_escrow(&JobId("j".into()), 100).unwrap();
        assert!(s
            .settle(
                &h,
                &SettlementOutcome {
                    result_hash: [0u8; 32],
                    base: 0,
                    winner: crate::types::Payout {
                        to: WalletAddress::new(0, [0u8; 32]),
                        amount: 0
                    },
                    participants: vec![],
                    platform_fee: 0,
                }
            )
            .is_ok());
        assert!(s.refund(&h).is_ok());
    }

    #[test]
    fn noop_stake_has_no_factor() {
        let r = NoopStakeRegistry;
        assert_eq!(r.stake_factor(&NodeId("x".into())), 0.0);
        assert!(r.is_eligible(&NodeId("x".into()), DataClassCfg::Public));
        assert!(!r.is_eligible(&NodeId("x".into()), DataClassCfg::Sensitive));
    }
}
