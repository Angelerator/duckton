//! `[economics]` — the blockchain economic / settlement layer configuration
//! (see `docs/BLOCKCHAIN_ECONOMICS.md` §12). Entirely **additive**: every value
//! has a documented default and the master switch defaults to `false`, so a
//! freshly-defaulted node behaves exactly as today (free, no chain).
//!
//! ## Free vs paid (decoupled from scoring)
//!
//! Settlement (chain + fees) is **optional and per-job**. Scoring
//! (reputation/quality) is **independent and always runs**. The per-job payment
//! mode is resolved with this precedence (highest first):
//!   1. per-call SQL override (`payment => 'free'|'paid'|'auto'` on `p2p_query`),
//!   2. data-class policy (`auto` ⇒ public→free, internal/sensitive→paid),
//!   3. the `[economics].default_payment` config default,
//!   4. the global `[economics].enabled` master switch (`false` ⇒ always free).
//!
//! A `free` job opens NO escrow, engages NO stake/settlement/anchor, pays NO
//! fees, and never reaches a TON client — yet still updates reputation/quality.

use serde::{Deserialize, Serialize};

use crate::{ConfigError, DataClassCfg};

/// Per-call / default payment preference (`payment => free|paid|auto`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PaymentPref {
    /// Force the free, no-chain path (no escrow/stake/anchor/fees).
    Free,
    /// Force the paid path (escrow + settlement + on-chain anchoring).
    Paid,
    /// Decide from data class: public ⇒ free, internal/sensitive ⇒ paid.
    Auto,
}

/// The resolved, per-job payment mode actually used by the coordinator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PaymentMode {
    /// No blockchain interaction whatsoever for this job.
    Free,
    /// Escrow + settlement + on-chain anchoring engaged.
    Paid,
}

impl PaymentMode {
    pub fn is_paid(self) -> bool {
        matches!(self, PaymentMode::Paid)
    }
    pub fn is_free(self) -> bool {
        matches!(self, PaymentMode::Free)
    }
}

/// Settlement rail selector (`[economics].settlement`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SettlementRail {
    /// No-op rail — today's free grid (never reaches a TON client).
    Noop,
    /// TON payment channels (gas-free per-job settlement).
    Channel,
    /// Direct per-job on-chain escrow.
    Onchain,
}

/// Top-level `[economics]` section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EconomicsConfig {
    /// Master switch. `false` (default) ⇒ today's free grid: no chain at all.
    pub enabled: bool,
    /// Settlement rail used by PAID jobs.
    pub settlement: SettlementRail,
    /// Custody model — v1 is strictly non-custodial (code-governed contracts).
    pub custody: String,
    /// Accounting unit — v1 prices AND settles natively in TON (no USD peg).
    pub accounting_unit: String,
    pub chain: String,
    pub network: String,
    /// Default payment preference when a call does not specify one.
    pub default_payment: PaymentPref,
    /// Configurable platform fee-recipient (treasury) address. `None` is allowed
    /// only when no on-chain fees are collected (free grid / `enabled = false`).
    pub fee_recipient: Option<String>,
    pub stake: StakeEconomics,
    pub ranking: RankingEconomics,
    pub quality: QualityEconomics,
    pub selection: SelectionEconomics,
    pub fees: FeesEconomics,
    pub slashing: SlashingEconomics,
    pub records: RecordsEconomics,
}

impl Default for EconomicsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            settlement: SettlementRail::Noop,
            custody: "noncustodial".to_string(),
            accounting_unit: "ton".to_string(),
            chain: "ton".to_string(),
            network: "mainnet".to_string(),
            default_payment: PaymentPref::Auto,
            fee_recipient: None,
            stake: StakeEconomics::default(),
            ranking: RankingEconomics::default(),
            quality: QualityEconomics::default(),
            selection: SelectionEconomics::default(),
            fees: FeesEconomics::default(),
            slashing: SlashingEconomics::default(),
            records: RecordsEconomics::default(),
        }
    }
}

/// `[economics.stake]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StakeEconomics {
    /// 0 = permissionless/free tier (public jobs only). Whole TON units.
    pub min_stake: u64,
    pub min_stake_internal: u64,
    pub min_stake_sensitive: u64,
    /// Ranking ceiling (anti-centralization). Whole TON units.
    pub stake_cap: u64,
    /// Unbonding cooldown; MUST be >= `slashing.challenge_window_secs`.
    pub unbonding_secs: u64,
    /// Mint a 1:1 TEP-74 stake-receipt jetton on deposit (§8.5).
    pub receipt_jetton: bool,
    /// Receipt is non-transferable while bonded/unbonding (anti-exit).
    pub receipt_transfer_locked: bool,
}

impl Default for StakeEconomics {
    fn default() -> Self {
        Self {
            min_stake: 0,
            min_stake_internal: 100,
            min_stake_sensitive: 1000,
            stake_cap: 100_000,
            unbonding_secs: 604_800,
            receipt_jetton: true,
            receipt_transfer_locked: true,
        }
    }
}

/// `[economics.ranking]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RankingEconomics {
    pub w_quality: f64,
    pub w_stake: f64,
    pub w_price: f64,
}

impl Default for RankingEconomics {
    fn default() -> Self {
        Self { w_quality: 0.6, w_stake: 0.15, w_price: 0.25 }
    }
}

/// `[economics.quality]` — weights for the provider quality score `Q` (§4.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QualityEconomics {
    pub w_success: f64,
    pub w_latency: f64,
    pub w_throughput: f64,
    pub w_completion: f64,
    pub latency_ref_ms: u64,
    pub bytes_ref: u64,
}

impl Default for QualityEconomics {
    fn default() -> Self {
        Self {
            w_success: 0.5,
            w_latency: 0.2,
            w_throughput: 0.2,
            w_completion: 0.1,
            latency_ref_ms: 5000,
            bytes_ref: 1_073_741_824,
        }
    }
}

/// `[economics.selection]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SelectionEconomics {
    pub n_public: usize,
    pub n_default: usize,
    pub n_max: usize,
    pub requester_overridable: bool,
    pub checksum_min: usize,
    pub checksum_allow_degraded: bool,
}

impl Default for SelectionEconomics {
    fn default() -> Self {
        Self {
            n_public: 3,
            n_default: 5,
            n_max: 10,
            requester_overridable: true,
            checksum_min: 3,
            checksum_allow_degraded: true,
        }
    }
}

/// `[economics.fees]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FeesEconomics {
    pub platform_fee_pct: f64,
    pub verification_surcharge_pct: f64,
    /// κ: fixed contract cut of the winner payout per agreeing non-winner.
    pub participation_commission_frac: f64,
    /// ρ: how much escrow slack funds the perf bonus.
    pub bonus_aggressiveness: f64,
    pub lambda_quality: f64,
    pub lambda_speed: f64,
}

impl Default for FeesEconomics {
    fn default() -> Self {
        Self {
            platform_fee_pct: 0.02,
            verification_surcharge_pct: 0.05,
            participation_commission_frac: 0.02,
            bonus_aggressiveness: 0.5,
            lambda_quality: 0.5,
            lambda_speed: 0.5,
        }
    }
}

/// `[economics.slashing]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SlashingEconomics {
    pub slash_wrong_result_pct: f64,
    pub slash_cheat_pct: f64,
    pub slash_downtime_pct: f64,
    pub slash_equivocation_pct: f64,
    pub challenge_window_secs: u64,
    pub slash_to_challenger: f64,
    pub slash_to_redundancy: f64,
    pub slash_to_burn: f64,
    pub slash_to_treasury: f64,
}

impl Default for SlashingEconomics {
    fn default() -> Self {
        Self {
            slash_wrong_result_pct: 0.15,
            slash_cheat_pct: 1.0,
            slash_downtime_pct: 0.02,
            slash_equivocation_pct: 0.5,
            challenge_window_secs: 86_400,
            slash_to_challenger: 0.4,
            slash_to_redundancy: 0.3,
            slash_to_burn: 0.2,
            slash_to_treasury: 0.1,
        }
    }
}

/// `[economics.records]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RecordsEconomics {
    pub epoch_secs: u64,
    pub anchor_quorum_pct: f64,
}

impl Default for RecordsEconomics {
    fn default() -> Self {
        Self { epoch_secs: 60, anchor_quorum_pct: 0.66 }
    }
}

impl EconomicsConfig {
    /// Resolve the per-job payment mode from the (already layered) config and the
    /// job's data class. Per-call overrides are folded into `default_payment` by
    /// [`crate::QueryOverrides::apply`] before this is called, so this is the
    /// single resolution point. The global `enabled` switch always wins: when the
    /// chain is off, every job is free.
    pub fn resolve_payment(&self, class: DataClassCfg) -> PaymentMode {
        if !self.enabled {
            return PaymentMode::Free;
        }
        match self.default_payment {
            PaymentPref::Free => PaymentMode::Free,
            PaymentPref::Paid => PaymentMode::Paid,
            PaymentPref::Auto => match class {
                DataClassCfg::Public => PaymentMode::Free,
                DataClassCfg::Internal | DataClassCfg::Sensitive => PaymentMode::Paid,
            },
        }
    }

    /// Validate the cross-field invariants from §12.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let inv = |m: String| Err(ConfigError::Invalid(m));
        let pct = |name: &str, x: f64| -> Result<(), ConfigError> {
            if !(0.0..=1.0).contains(&x) {
                return Err(ConfigError::Invalid(format!("economics.{name} must be in [0,1], got {x}")));
            }
            Ok(())
        };

        if !matches!(self.custody.as_str(), "noncustodial") {
            return inv(format!("economics.custody must be \"noncustodial\" (v1), got {}", self.custody));
        }
        if !matches!(self.accounting_unit.as_str(), "ton") {
            return inv(format!("economics.accounting_unit must be \"ton\" (v1), got {}", self.accounting_unit));
        }

        // Unbonding must outlast the challenge window so a cheater can't exit
        // before a fraud proof lands (§8.4).
        if self.stake.unbonding_secs < self.slashing.challenge_window_secs {
            return inv(format!(
                "economics.stake.unbonding_secs ({}) must be >= economics.slashing.challenge_window_secs ({})",
                self.stake.unbonding_secs, self.slashing.challenge_window_secs
            ));
        }

        // Per-class stake minimums must be monotonic.
        if !(self.stake.min_stake_sensitive >= self.stake.min_stake_internal
            && self.stake.min_stake_internal >= self.stake.min_stake)
        {
            return inv("economics.stake: require min_stake_sensitive >= min_stake_internal >= min_stake".into());
        }
        if self.stake.stake_cap < self.stake.min_stake {
            return inv("economics.stake.stake_cap must be >= min_stake".into());
        }

        // Slash split must sum to 1.0.
        let s = &self.slashing;
        let sum = s.slash_to_challenger + s.slash_to_redundancy + s.slash_to_burn + s.slash_to_treasury;
        if (sum - 1.0).abs() > 1e-9 {
            return inv(format!("economics.slashing slash_to_* must sum to 1.0, got {sum}"));
        }
        pct("slashing.slash_wrong_result_pct", s.slash_wrong_result_pct)?;
        pct("slashing.slash_cheat_pct", s.slash_cheat_pct)?;
        pct("slashing.slash_downtime_pct", s.slash_downtime_pct)?;
        pct("slashing.slash_equivocation_pct", s.slash_equivocation_pct)?;
        pct("slashing.slash_to_challenger", s.slash_to_challenger)?;
        pct("slashing.slash_to_redundancy", s.slash_to_redundancy)?;
        pct("slashing.slash_to_burn", s.slash_to_burn)?;
        pct("slashing.slash_to_treasury", s.slash_to_treasury)?;

        // Fees.
        pct("fees.platform_fee_pct", self.fees.platform_fee_pct)?;
        pct("fees.verification_surcharge_pct", self.fees.verification_surcharge_pct)?;
        if !(0.0..=0.1).contains(&self.fees.participation_commission_frac) {
            return inv(format!(
                "economics.fees.participation_commission_frac must be in [0,0.1], got {}",
                self.fees.participation_commission_frac
            ));
        }
        pct("fees.bonus_aggressiveness", self.fees.bonus_aggressiveness)?;
        if (self.fees.lambda_quality + self.fees.lambda_speed - 1.0).abs() > 1e-9 {
            return inv("economics.fees: lambda_quality + lambda_speed must sum to 1.0".into());
        }

        // Ranking weights must be non-negative.
        let r = &self.ranking;
        if r.w_quality < 0.0 || r.w_stake < 0.0 || r.w_price < 0.0 {
            return inv("economics.ranking weights must be >= 0".into());
        }

        // Selection ordering.
        let sel = &self.selection;
        if sel.checksum_min < 1 {
            return inv("economics.selection.checksum_min must be >= 1".into());
        }
        if !(sel.n_max >= sel.n_default && sel.n_default >= sel.n_public && sel.n_public >= sel.checksum_min) {
            return inv(
                "economics.selection: require n_max >= n_default >= n_public >= checksum_min".into(),
            );
        }

        // Records.
        pct("records.anchor_quorum_pct", self.records.anchor_quorum_pct)?;
        if self.records.epoch_secs == 0 {
            return inv("economics.records.epoch_secs must be >= 1".into());
        }

        // A fee recipient is required once on-chain fees can actually be charged.
        if self.enabled && !matches!(self.settlement, SettlementRail::Noop) {
            match &self.fee_recipient {
                Some(a) if !a.trim().is_empty() => {}
                _ => {
                    return inv(
                        "economics.fee_recipient must be set when economics.enabled and settlement != noop".into(),
                    )
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_free() {
        let e = EconomicsConfig::default();
        e.validate().unwrap();
        assert!(!e.enabled);
        // disabled ⇒ every job is free regardless of class
        assert_eq!(e.resolve_payment(DataClassCfg::Sensitive), PaymentMode::Free);
    }

    #[test]
    fn auto_routes_public_free_and_sensitive_paid_when_enabled() {
        let mut e = EconomicsConfig::default();
        e.enabled = true;
        e.fee_recipient = Some("EQ...treasury".into());
        e.settlement = SettlementRail::Channel;
        e.default_payment = PaymentPref::Auto;
        e.validate().unwrap();
        assert_eq!(e.resolve_payment(DataClassCfg::Public), PaymentMode::Free);
        assert_eq!(e.resolve_payment(DataClassCfg::Internal), PaymentMode::Paid);
        assert_eq!(e.resolve_payment(DataClassCfg::Sensitive), PaymentMode::Paid);
    }

    #[test]
    fn explicit_pref_overrides_class() {
        let mut e = EconomicsConfig::default();
        e.enabled = true;
        e.fee_recipient = Some("EQ...treasury".into());
        e.settlement = SettlementRail::Channel;
        e.default_payment = PaymentPref::Free;
        assert_eq!(e.resolve_payment(DataClassCfg::Sensitive), PaymentMode::Free);
        e.default_payment = PaymentPref::Paid;
        assert_eq!(e.resolve_payment(DataClassCfg::Public), PaymentMode::Paid);
    }

    #[test]
    fn rejects_unbonding_shorter_than_challenge_window() {
        let mut e = EconomicsConfig::default();
        e.stake.unbonding_secs = 100;
        e.slashing.challenge_window_secs = 200;
        assert!(e.validate().is_err());
    }

    #[test]
    fn rejects_slash_split_not_summing_to_one() {
        let mut e = EconomicsConfig::default();
        e.slashing.slash_to_burn = 0.5; // now sums to 1.3
        assert!(e.validate().is_err());
    }

    #[test]
    fn requires_fee_recipient_when_charging_onchain() {
        let mut e = EconomicsConfig::default();
        e.enabled = true;
        e.settlement = SettlementRail::Onchain;
        e.fee_recipient = None;
        assert!(e.validate().is_err());
    }
}
