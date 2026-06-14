//! `p2p-settlement` — the optional blockchain economic / settlement layer for the
//! DuckDB-over-QUIC grid (see `docs/BLOCKCHAIN_ECONOMICS.md`).
//!
//! It exposes the four pluggable seams from §10.1 — [`Wallet`], [`Settlement`],
//! [`StakeRegistry`], [`RecordAnchor`] — each with three implementations:
//!
//! * **`mock`** — deterministic, in-memory, exercises the full PAID path with no
//!   network (the default test doubles).
//! * **`noop`** — a genuine no-op, the FREE / no-chain path: opens no escrow,
//!   engages no stake/settlement/anchor, pays no fees, and never reaches a TON
//!   client. Selected per-job for `payment = free`.
//! * **`ton`** — talks to the on-chain contracts in `ton/` (Tolk/Acton). Message
//!   ABI + `ton_proof` + Merkle proofs are unit-tested; live RPC is gated behind
//!   the `ton-live` feature.
//!
//! ## Settlement vs scoring are decoupled
//!
//! This crate only concerns **settlement** (chain + fees), which is optional and
//! per-job. **Scoring** (reputation/quality) lives in `p2p-trust` and the
//! coordinator and ALWAYS runs — a free job is dispatched, quorum/canary-verified,
//! produces signed receipts, and updates reputation exactly like a paid job. The
//! `stake_factor` seam is the only economics-gated input to the trust score.

pub mod cell;
pub mod engagement;
pub mod merkle;
pub mod mock;
pub mod noop;
pub mod quality;
pub mod ton;
pub mod ton_proof;
pub mod traits;
pub mod types;
pub mod wallet;
pub mod wiring;

pub use cell::{Cell, CellBuilder, BASECHAIN};
pub use engagement::settle_if_paid;
pub use wallet::{
    build_signed_external_v5r1, InternalMessage, WalletKey, WalletV5R1, GLOBAL_ID_MAINNET,
    GLOBAL_ID_TESTNET,
};
pub use quality::{latency_score, quality_score, throughput_score, QualitySample};
pub use ton::{
    build_anchor_submit, build_escrow_refund, build_escrow_settle, build_escrow_topup,
    build_stake_deposit, build_stake_slash, build_stake_unbond, build_update_admin,
    build_update_params, GlobalParams, MessageBody, TonRecordAnchor, TonRpc, TonSettlement,
    TonStakeRegistry, VaultInit,
};
#[cfg(feature = "ton-live")]
pub use ton::ToncenterRpc;
pub use traits::{RecordAnchor, Settlement, StakeRegistry, Wallet};
pub use wiring::{resolve_ton_wiring, TonWiring};
pub use types::{
    Amount, BindingError, EscrowHandle, Hash32, InclusionProof, JobRecord, NodeWalletBinding,
    Payout, SettleError, SettlementOutcome, SlashError, SlashReason, TonProof, WalletAddress,
};

pub use mock::{
    InMemoryRecordAnchor, InMemoryStakeRegistry, MockSettlement, MockWallet, SettlementEvent,
};
pub use noop::{NoopRecordAnchor, NoopSettlement, NoopStakeRegistry, NoopWallet};
pub use ton_proof::{verify_binding, verify_ton_proof};

// Re-export the per-job payment mode types so consumers have one import surface.
pub use p2p_config::{PaymentMode, PaymentPref};

/// Diminishing, capped stake-ranking factor in `[0,1]` (BLOCKCHAIN_ECONOMICS §5.2):
///
/// ```text
/// StakeFactor = min(1, ln(1 + stake/min) / ln(1 + cap/min))
/// ```
///
/// Logarithmic growth + a hard `stake_cap` means doubling stake yields
/// ever-smaller gains and beyond the cap yields nothing — the explicit
/// anti-"rich-get-richer" guardrail. When `min_stake == 0` (free tier) a 1-TON
/// scale is used so the curve is still well-defined.
pub fn stake_factor(stake: Amount, min_stake: Amount, stake_cap: Amount) -> f64 {
    if stake == 0 {
        return 0.0;
    }
    const ONE_TON: Amount = 1_000_000_000;
    let base = if min_stake == 0 { ONE_TON } else { min_stake };
    let cap = stake_cap.max(base);
    let num = (1.0 + stake as f64 / base as f64).ln();
    let den = (1.0 + cap as f64 / base as f64).ln();
    if den <= 0.0 {
        return 0.0;
    }
    (num / den).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TON: Amount = 1_000_000_000;

    #[test]
    fn stake_factor_is_zero_at_zero_and_capped_at_one() {
        assert_eq!(stake_factor(0, 100 * TON, 100_000 * TON), 0.0);
        // at/above the cap it saturates to 1.0
        assert!((stake_factor(100_000 * TON, 100 * TON, 100_000 * TON) - 1.0).abs() < 1e-9);
        assert!((stake_factor(1_000_000 * TON, 100 * TON, 100_000 * TON) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn stake_factor_is_diminishing() {
        let min = 100 * TON;
        let cap = 100_000 * TON;
        // Equal absolute increments of stake (+100 TON each) yield diminishing
        // ranking gains because the factor is logarithmic.
        let f1 = stake_factor(100 * TON, min, cap);
        let f2 = stake_factor(200 * TON, min, cap);
        let f3 = stake_factor(300 * TON, min, cap);
        // monotonic increasing, but with diminishing increments
        assert!(f2 > f1 && f3 > f2);
        assert!((f2 - f1) > (f3 - f2), "increments must diminish");
    }

    #[test]
    fn stake_factor_handles_free_tier_min_zero() {
        // min_stake = 0 must not panic and stays in range.
        let f = stake_factor(50 * TON, 0, 100_000 * TON);
        assert!((0.0..=1.0).contains(&f));
    }
}
