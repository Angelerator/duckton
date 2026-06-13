//! Per-job free/paid engagement (BLOCKCHAIN_ECONOMICS §8.2).
//!
//! This is the single decision point that keeps settlement **optional and
//! per-job**: a FREE job NEVER touches the settlement rail, a PAID job opens the
//! escrow and settles. Scoring (reputation/quality) is handled elsewhere and is
//! unaffected by this function.

use p2p_proto::JobId;

use crate::traits::Settlement;
use crate::types::{Amount, SettlementOutcome, SettleError};
use crate::PaymentMode;

/// Engage settlement for PAID jobs only.
///
/// * `PaymentMode::Free` ⇒ returns `Ok(false)` **without calling `settlement`**
///   at all — genuinely zero chain interaction (works even with a `NoopSettlement`
///   or no chain configured).
/// * `PaymentMode::Paid` ⇒ opens the per-job escrow and settles per the quorum
///   outcome, returning `Ok(true)`.
pub fn settle_if_paid(
    mode: PaymentMode,
    settlement: &dyn Settlement,
    job: &JobId,
    max_bid: Amount,
    outcome: &SettlementOutcome,
) -> Result<bool, SettleError> {
    if mode.is_free() {
        return Ok(false);
    }
    let handle = settlement.open_escrow(job, max_bid)?;
    settlement.settle(&handle, outcome)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockSettlement;
    use crate::noop::NoopSettlement;
    use crate::types::{Payout, WalletAddress};

    fn sample_outcome() -> SettlementOutcome {
        SettlementOutcome {
            result_hash: [7u8; 32],
            winner: Payout { to: WalletAddress::new(0, [1u8; 32]), amount: 60 },
            participants: vec![Payout { to: WalletAddress::new(0, [2u8; 32]), amount: 2 }],
            platform_fee: 2,
        }
    }

    /// A settlement spy that PANICS on any call — proves a FREE job never engages it.
    struct PanicSettlement;
    impl Settlement for PanicSettlement {
        fn open_escrow(&self, _: &JobId, _: Amount) -> Result<crate::EscrowHandle, SettleError> {
            panic!("settlement engaged on a FREE job (open_escrow)");
        }
        fn settle(&self, _: &crate::EscrowHandle, _: &SettlementOutcome) -> Result<(), SettleError> {
            panic!("settlement engaged on a FREE job (settle)");
        }
        fn refund(&self, _: &crate::EscrowHandle) -> Result<(), SettleError> {
            panic!("settlement engaged on a FREE job (refund)");
        }
        fn is_onchain(&self) -> bool {
            panic!("settlement inspected on a FREE job (is_onchain)");
        }
    }

    #[test]
    fn free_job_never_touches_settlement() {
        // Even with a spy that panics on ANY call, a free job completes cleanly.
        let engaged =
            settle_if_paid(PaymentMode::Free, &PanicSettlement, &JobId("j".into()), 100, &sample_outcome())
                .unwrap();
        assert!(!engaged, "free job must not engage settlement");
    }

    #[test]
    fn free_job_works_with_noop_rail() {
        let engaged =
            settle_if_paid(PaymentMode::Free, &NoopSettlement, &JobId("j".into()), 100, &sample_outcome())
                .unwrap();
        assert!(!engaged);
    }

    #[test]
    fn paid_job_engages_settlement() {
        let mock = MockSettlement::new();
        let engaged =
            settle_if_paid(PaymentMode::Paid, &mock, &JobId("j".into()), 100, &sample_outcome()).unwrap();
        assert!(engaged, "paid job must engage settlement");
        // open_escrow + settle were both recorded.
        assert_eq!(mock.call_count(), 2);
    }
}
