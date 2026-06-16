//! Deterministic in-memory implementations for tests and single-process nodes.
//! These exercise the FULL paid path (escrow open/settle/refund, stake, anchor)
//! without any network — the default test doubles.

use std::collections::HashMap;
use std::sync::Mutex;

use p2p_config::DataClassCfg;
use p2p_proto::{JobId, NodeId};

use crate::stake_factor;
use crate::traits::{RecordAnchor, Settlement, StakeRegistry, Wallet};
use crate::types::{
    Amount, EscrowHandle, Hash32, InclusionProof, JobRecord, NodeWalletBinding, SettleError,
    SettlementOutcome, SlashError, SlashReason, WalletAddress,
};

// ---------------------------------------------------------------------------
// Wallet
// ---------------------------------------------------------------------------

/// A wallet whose binding verification uses the real `ton_proof` code path.
pub struct MockWallet {
    address: WalletAddress,
}

impl MockWallet {
    pub fn new(address: WalletAddress) -> Self {
        Self { address }
    }
}

impl Wallet for MockWallet {
    fn address(&self) -> WalletAddress {
        self.address
    }
    fn verify_binding(&self, b: &NodeWalletBinding, now: u64) -> bool {
        crate::ton_proof::verify_binding(b, now).is_ok()
    }
}

// ---------------------------------------------------------------------------
// StakeRegistry
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
struct StakeEntry {
    staked: Amount,
    unbonding: Amount,
}

/// In-memory stake registry implementing the diminishing/capped `stake_factor`.
pub struct InMemoryStakeRegistry {
    inner: Mutex<HashMap<NodeId, StakeEntry>>,
    /// Per-class minimum stake (nanoton).
    min_public: Amount,
    min_internal: Amount,
    min_sensitive: Amount,
    /// Ranking cap (nanoton).
    stake_cap: Amount,
}

impl InMemoryStakeRegistry {
    pub fn new(
        min_public: Amount,
        min_internal: Amount,
        min_sensitive: Amount,
        stake_cap: Amount,
    ) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            min_public,
            min_internal,
            min_sensitive,
            stake_cap,
        }
    }

    /// Convenience constructor from the `[economics.stake]` config (whole TON).
    pub fn from_config(stake: &p2p_config::StakeEconomics) -> Self {
        const TON: Amount = 1_000_000_000;
        Self::new(
            stake.min_stake as Amount * TON,
            stake.min_stake_internal as Amount * TON,
            stake.min_stake_sensitive as Amount * TON,
            stake.stake_cap as Amount * TON,
        )
    }

    /// Test/admin helper: set a node's bonded stake (nanoton).
    pub fn set_stake(&self, node: &NodeId, amount: Amount) {
        let mut g = self.inner.lock().unwrap();
        g.entry(node.clone()).or_default().staked = amount;
    }

    fn min_for(&self, class: DataClassCfg) -> Amount {
        match class {
            DataClassCfg::Public => self.min_public,
            DataClassCfg::Internal => self.min_internal,
            DataClassCfg::Sensitive => self.min_sensitive,
        }
    }
}

impl StakeRegistry for InMemoryStakeRegistry {
    fn stake_of(&self, node: &NodeId) -> Amount {
        self.inner
            .lock()
            .unwrap()
            .get(node)
            .map(|e| e.staked)
            .unwrap_or(0)
    }

    fn is_eligible(&self, node: &NodeId, class: DataClassCfg) -> bool {
        self.stake_of(node) >= self.min_for(class)
    }

    fn stake_factor(&self, node: &NodeId) -> f64 {
        stake_factor(self.stake_of(node), self.min_public, self.stake_cap)
    }

    fn slash(&self, node: &NodeId, _reason: SlashReason, amount: Amount) -> Result<(), SlashError> {
        let mut g = self.inner.lock().unwrap();
        let e = g
            .get_mut(node)
            .ok_or_else(|| SlashError::UnknownNode(node.0.clone()))?;
        let slashable = e.staked + e.unbonding;
        if amount > slashable {
            return Err(SlashError::ExceedsStake { amount, slashable });
        }
        let from_staked = amount.min(e.staked);
        e.staked -= from_staked;
        e.unbonding -= amount - from_staked;
        Ok(())
    }

    fn request_unbond(&self, node: &NodeId, amount: Amount) -> Result<(), SlashError> {
        let mut g = self.inner.lock().unwrap();
        let e = g
            .get_mut(node)
            .ok_or_else(|| SlashError::UnknownNode(node.0.clone()))?;
        if amount > e.staked {
            return Err(SlashError::ExceedsStake {
                amount,
                slashable: e.staked,
            });
        }
        e.staked -= amount;
        e.unbonding += amount;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Settlement
// ---------------------------------------------------------------------------

/// One recorded settlement action (for test assertions).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettlementEvent {
    Opened { job: JobId, max_bid: Amount },
    Settled { job: JobId, total: Amount },
    Refunded { job: JobId },
}

/// In-memory escrow that records every action. Exercises the paid path with no
/// network. `is_onchain()` is `true` so callers can assert it was engaged.
pub struct MockSettlement {
    events: Mutex<Vec<SettlementEvent>>,
}

impl Default for MockSettlement {
    fn default() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
        }
    }
}

impl MockSettlement {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn events(&self) -> Vec<SettlementEvent> {
        self.events.lock().unwrap().clone()
    }
    pub fn call_count(&self) -> usize {
        self.events.lock().unwrap().len()
    }
}

fn escrow_address_for(job: &JobId) -> WalletAddress {
    WalletAddress::new(0, *blake3::hash(job.0.as_bytes()).as_bytes())
}

impl Settlement for MockSettlement {
    fn open_escrow(&self, job: &JobId, max_bid: Amount) -> Result<EscrowHandle, SettleError> {
        self.events.lock().unwrap().push(SettlementEvent::Opened {
            job: job.clone(),
            max_bid,
        });
        Ok(EscrowHandle {
            job: job.clone(),
            address: escrow_address_for(job),
            max_bid,
        })
    }

    fn settle(&self, h: &EscrowHandle, outcome: &SettlementOutcome) -> Result<(), SettleError> {
        let total = outcome.total();
        if total > h.max_bid {
            return Err(SettleError::PayoutExceedsEscrow {
                payout: total,
                escrow: h.max_bid,
            });
        }
        self.events.lock().unwrap().push(SettlementEvent::Settled {
            job: h.job.clone(),
            total,
        });
        Ok(())
    }

    fn refund(&self, h: &EscrowHandle) -> Result<(), SettleError> {
        self.events
            .lock()
            .unwrap()
            .push(SettlementEvent::Refunded { job: h.job.clone() });
        Ok(())
    }

    fn is_onchain(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// RecordAnchor
// ---------------------------------------------------------------------------

/// In-memory epoch tree: appends leaves, computes the BLAKE3 root, proves inclusion.
pub struct InMemoryRecordAnchor {
    inner: Mutex<Vec<(JobId, Hash32)>>,
}

impl Default for InMemoryRecordAnchor {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Vec::new()),
        }
    }
}

impl InMemoryRecordAnchor {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl RecordAnchor for InMemoryRecordAnchor {
    fn append(&self, record: &JobRecord) {
        let job = JobId(record.job_id.clone());
        self.inner.lock().unwrap().push((job, record.leaf()));
    }

    fn epoch_root(&self) -> Hash32 {
        let g = self.inner.lock().unwrap();
        let leaves: Vec<Hash32> = g.iter().map(|(_, h)| *h).collect();
        crate::merkle::merkle_root(&leaves)
    }

    fn prove_inclusion(&self, job: &JobId) -> Option<InclusionProof> {
        let g = self.inner.lock().unwrap();
        let idx = g.iter().position(|(j, _)| j == job)?;
        let leaves: Vec<Hash32> = g.iter().map(|(_, h)| *h).collect();
        crate::merkle::build_proof(&leaves, idx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TON: Amount = 1_000_000_000;

    #[test]
    fn stake_registry_eligibility_and_factor() {
        let reg = InMemoryStakeRegistry::new(0, 100 * TON, 1000 * TON, 100_000 * TON);
        let n = NodeId("b3:a".into());
        // free tier: eligible for public with zero stake, not for internal.
        assert!(reg.is_eligible(&n, DataClassCfg::Public));
        assert!(!reg.is_eligible(&n, DataClassCfg::Internal));
        assert_eq!(reg.stake_factor(&n), 0.0);

        reg.set_stake(&n, 1000 * TON);
        assert!(reg.is_eligible(&n, DataClassCfg::Internal));
        let f = reg.stake_factor(&n);
        assert!(f > 0.0 && f <= 1.0);
    }

    #[test]
    fn mock_settlement_records_and_bounds() {
        let s = MockSettlement::new();
        let job = JobId("job1".into());
        let h = s.open_escrow(&job, 100 * TON).unwrap();
        let outcome = SettlementOutcome {
            result_hash: [1u8; 32],
            base: 60 * TON,
            winner: crate::types::Payout {
                to: WalletAddress::new(0, [2u8; 32]),
                amount: 60 * TON,
            },
            participants: vec![],
            platform_fee: 2 * TON,
        };
        s.settle(&h, &outcome).unwrap();
        assert_eq!(s.call_count(), 2);

        // Over-budget settle is rejected.
        let too_big = SettlementOutcome {
            result_hash: [1u8; 32],
            base: 200 * TON,
            winner: crate::types::Payout {
                to: WalletAddress::new(0, [2u8; 32]),
                amount: 200 * TON,
            },
            participants: vec![],
            platform_fee: 0,
        };
        assert!(s.settle(&h, &too_big).is_err());
    }

    #[test]
    fn anchor_proves_inclusion() {
        let a = InMemoryRecordAnchor::new();
        for i in 0..3u8 {
            a.append(&JobRecord {
                job_id: format!("job{i}"),
                query_hash: "q".into(),
                requester_wallet: "0:00".into(),
                max_bid: 1,
                result_hash: "r".into(),
                epoch: 1,
                prev_root: [0u8; 32],
                params_version: 0,
            });
        }
        let root = a.epoch_root();
        let proof = a.prove_inclusion(&JobId("job1".into())).unwrap();
        assert!(crate::merkle::verify_inclusion(&root, &proof));
    }
}
