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
///
/// The SQL surface exposes the business-friendly names `noop` | `mock` | `ton`;
/// `ton` maps to [`SettlementRail::Onchain`] (real per-job on-chain escrow).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SettlementRail {
    /// No-op rail — today's free grid (never reaches a TON client).
    Noop,
    /// TON payment channels (gas-free per-job settlement).
    Channel,
    /// Direct per-job on-chain escrow.
    Onchain,
    /// Deterministic in-memory **mock** rail: exercises the paid code path
    /// (escrow/settle/anchor) with NO chain and NO funds — for testing/dev.
    Mock,
}

/// TON network mode (`[economics].network`). **Testnet is the safe default.**
/// Selecting/transacting on **mainnet** requires an explicit confirmation
/// (`economics.mainnet_confirmed`, set via `p2p_economics(network => 'mainnet',
/// confirm => true)`), because real TON is at stake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum TonNetwork {
    /// TON testnet — free test coins, the safe default.
    #[default]
    Testnet,
    /// TON mainnet — **real funds**. Requires explicit opt-in.
    Mainnet,
}

impl TonNetwork {
    /// The canonical lowercase name (`"testnet"` | `"mainnet"`).
    pub fn as_str(self) -> &'static str {
        match self {
            TonNetwork::Testnet => "testnet",
            TonNetwork::Mainnet => "mainnet",
        }
    }

    /// The default toncenter RPC endpoint for this network (overridable via the
    /// per-network `rpc` setting).
    pub fn default_rpc(self) -> &'static str {
        match self {
            TonNetwork::Testnet => "https://testnet.toncenter.com/api/v2/",
            TonNetwork::Mainnet => "https://toncenter.com/api/v2/",
        }
    }

    /// The default explorer host for this network (used for status/links;
    /// overridable via the per-network `explorer` setting).
    pub fn default_explorer(self) -> &'static str {
        match self {
            TonNetwork::Testnet => "testnet.tonviewer.com",
            TonNetwork::Mainnet => "tonviewer.com",
        }
    }
}

/// Deployed contract addresses for one network (`[economics.<net>.contracts]`).
/// All optional — registered via `p2p_contracts(...)` once contracts are
/// deployed. Stored **per network** so switching `network` flips to that
/// network's addresses without reconfiguring.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContractsConfig {
    /// `StakeVault` contract address (provider staking).
    pub stake_vault: Option<String>,
    /// `JobEscrow` factory/template address (per-job escrow).
    pub job_escrow: Option<String>,
    /// `RecordAnchor` contract address (epoch Merkle-root anchoring).
    pub record_anchor: Option<String>,
    /// `GlobalParams` contract address (platform-wide economic params, §12). Its
    /// address is **stable** (params are edited in place), so it is safe to pin.
    pub global_params: Option<String>,
}

/// Wallet **references** for one network (`[economics.<net>.wallet]`).
///
/// SECURITY: this NEVER stores a raw mnemonic / API key. It stores only a public
/// address plus a path reference to a `0600` secret file kept OUTSIDE the repo.
/// The SQL surface redacts secrets everywhere and writes raw inline secrets to a
/// protected file, persisting only the reference here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WalletConfig {
    /// Public wallet address (safe to display).
    pub address: Option<String>,
    /// Path to a `0600` file holding the mnemonic (NEVER the mnemonic itself).
    pub mnemonic_file: Option<String>,
}

/// Per-network settings (`[economics.testnet]` / `[economics.mainnet]`). Both
/// networks can be configured simultaneously; the active one is selected by
/// `economics.network`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkSettings {
    /// Toncenter RPC endpoint override. `None` ⇒ the per-network default
    /// ([`TonNetwork::default_rpc`]).
    pub rpc: Option<String>,
    /// Explorer host override. `None` ⇒ the per-network default
    /// ([`TonNetwork::default_explorer`]).
    pub explorer: Option<String>,
    /// Path to a `0600` file holding the toncenter API key (NEVER the key itself).
    pub api_key_file: Option<String>,
    pub contracts: ContractsConfig,
    pub wallet: WalletConfig,
}

/// Pricing knobs (`[economics.pricing]`). Role-appropriate: providers advertise
/// `unit_price`; requesters cap spend with `max_bid`. Whole-TON units; `0` =
/// unset (free / no cap).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PricingEconomics {
    /// Provider's advertised unit price (whole TON per reference unit). `0`=free.
    pub unit_price: u64,
    /// Requester's max bid / budget cap (whole TON). `0` = no cap.
    pub max_bid: u64,
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
    /// Active TON network mode. **Defaults to `testnet`** (safe). Switch to
    /// `mainnet` only with an explicit confirmation (see `mainnet_confirmed`).
    pub network: TonNetwork,
    /// Explicit opt-in that mainnet (real funds) is intended. Required before
    /// selecting/transacting on mainnet; set via
    /// `p2p_economics(network => 'mainnet', confirm => true)`.
    pub mainnet_confirmed: bool,
    /// Default payment preference when a call does not specify one.
    pub default_payment: PaymentPref,
    /// Configurable platform fee-recipient (treasury) address. `None` is allowed
    /// only when no on-chain fees are collected (free grid / `enabled = false`).
    pub fee_recipient: Option<String>,
    pub stake: StakeEconomics,
    pub ranking: RankingEconomics,
    pub quality: QualityEconomics,
    pub reputation: ReputationEconomics,
    pub selection: SelectionEconomics,
    pub fees: FeesEconomics,
    pub slashing: SlashingEconomics,
    pub records: RecordsEconomics,
    pub pricing: PricingEconomics,
    /// Per-network settings for testnet (addresses/endpoints/wallet refs).
    pub testnet: NetworkSettings,
    /// Per-network settings for mainnet (addresses/endpoints/wallet refs).
    pub mainnet: NetworkSettings,
}

impl Default for EconomicsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            settlement: SettlementRail::Noop,
            custody: "noncustodial".to_string(),
            accounting_unit: "ton".to_string(),
            chain: "ton".to_string(),
            network: TonNetwork::default(),
            mainnet_confirmed: false,
            default_payment: PaymentPref::Auto,
            fee_recipient: None,
            stake: StakeEconomics::default(),
            ranking: RankingEconomics::default(),
            quality: QualityEconomics::default(),
            reputation: ReputationEconomics::default(),
            selection: SelectionEconomics::default(),
            fees: FeesEconomics::default(),
            slashing: SlashingEconomics::default(),
            records: RecordsEconomics::default(),
            pricing: PricingEconomics::default(),
            testnet: NetworkSettings::default(),
            mainnet: NetworkSettings::default(),
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
    /// Cold-start exploration rate ε (BLOCKCHAIN_ECONOMICS §5.2/§6): an
    /// uncertainty bonus added to a candidate's selection score that decays as
    /// the node accrues verified observations, so brand-new honest nodes still
    /// get sampled and can build reputation. `0.0` (default) reproduces today's
    /// pure-exploitation ranking; a small value (e.g. `0.1`) enables exploration.
    pub exploration_rate: f64,
    /// Observation count at which the exploration bonus has fully decayed to 0
    /// (a node with this many verified jobs is no longer "new").
    pub exploration_saturation: usize,
}

impl Default for RankingEconomics {
    fn default() -> Self {
        Self {
            w_quality: 0.6,
            w_stake: 0.15,
            w_price: 0.25,
            exploration_rate: 0.0,
            exploration_saturation: 20,
        }
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
    /// Reference throughput (bytes/ms) for the **throughput-as-rate** term `T`
    /// (§4.1). `0` (default) ⇒ derive the reference rate from
    /// `bytes_ref / latency_ref_ms`, so a job that processes `bytes_ref` bytes in
    /// `latency_ref_ms` scores a neutral mid-range throughput.
    pub throughput_ref_bytes_per_ms: u64,
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
            throughput_ref_bytes_per_ms: 0,
        }
    }
}

/// `[economics.reputation]` — confidence-aware reputation priors (§4.1/§7.3).
///
/// The raw recency-weighted success ratio is replaced (at selection time) by a
/// **Beta/Wilson lower-confidence-bound** estimate so a node with only a handful
/// of verified jobs is NOT treated as fully trusted: a "3-for-3" newcomer scores
/// well below a node with a long correct history. The priors are pseudo-counts
/// (`prior_alpha` successes, `prior_beta` failures) added before computing the
/// Wilson lower bound at confidence `confidence_z` (z-score, e.g. 1.96 ≈ 95%).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReputationEconomics {
    /// Beta prior pseudo-successes (≥ 0).
    pub prior_alpha: f64,
    /// Beta prior pseudo-failures (≥ 0). A larger `prior_beta` ⇒ more pessimistic
    /// about thinly-observed nodes.
    pub prior_beta: f64,
    /// Wilson lower-bound confidence z-score (≥ 0). `0` collapses to the plain
    /// (prior-shrunk) Beta posterior mean with no interval widening.
    pub confidence_z: f64,
}

impl Default for ReputationEconomics {
    fn default() -> Self {
        // Mildly pessimistic prior (one pseudo-success, two pseudo-failures) plus
        // a ~95% Wilson lower bound: a brand-new correct node is sampled via the
        // exploration bonus rather than trusted outright.
        Self {
            prior_alpha: 1.0,
            prior_beta: 2.0,
            confidence_z: 1.96,
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
    /// Fine for a **broken commitment**: a provider that accepted/bid on a PAID
    /// job then failed to deliver a valid result by the deadline (no result /
    /// timeout / abandoned / wrong hash) WHILE the job was demonstrably feasible
    /// (a quorum was reached / another selected node delivered). Penalizes the
    /// broken commitment to paid work — distinct from a wrong-result slash.
    /// Fraction `[0,1]` of the provider's bonded stake. Configurable / graduated.
    pub slash_failed_commitment_pct: f64,
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
            slash_failed_commitment_pct: 0.1,
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
        Self {
            epoch_secs: 60,
            anchor_quorum_pct: 0.66,
        }
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

    /// The per-network settings block for the currently-active network.
    pub fn active_settings(&self) -> &NetworkSettings {
        match self.network {
            TonNetwork::Testnet => &self.testnet,
            TonNetwork::Mainnet => &self.mainnet,
        }
    }

    /// Effective RPC endpoint for the active network (override or default).
    pub fn resolved_rpc(&self) -> String {
        self.active_settings()
            .rpc
            .clone()
            .unwrap_or_else(|| self.network.default_rpc().to_string())
    }

    /// Effective explorer host for the active network (override or default).
    pub fn resolved_explorer(&self) -> String {
        self.active_settings()
            .explorer
            .clone()
            .unwrap_or_else(|| self.network.default_explorer().to_string())
    }

    /// True when the active network is **mainnet but not explicitly confirmed**.
    /// Mainnet switches/actions must be blocked in this state (real funds).
    pub fn mainnet_blocked(&self) -> bool {
        matches!(self.network, TonNetwork::Mainnet) && !self.mainnet_confirmed
    }

    /// Guard for paid / on-chain actions: returns a clear, actionable error when
    /// mainnet is selected without explicit confirmation.
    pub fn guard_mainnet(&self) -> Result<(), String> {
        if self.mainnet_blocked() {
            return Err(
                "mainnet is selected but NOT confirmed — real TON is at stake. Re-run \
                 `CALL p2p_economics(network => 'mainnet', confirm => true)` (or set \
                 economics.mainnet_confirmed = true) before any paid/on-chain action."
                    .to_string(),
            );
        }
        Ok(())
    }

    /// Validate the cross-field invariants from §12.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let inv = |m: String| Err(ConfigError::Invalid(m));
        let pct = |name: &str, x: f64| -> Result<(), ConfigError> {
            if !(0.0..=1.0).contains(&x) {
                return Err(ConfigError::Invalid(format!(
                    "economics.{name} must be in [0,1], got {x}"
                )));
            }
            Ok(())
        };

        if !matches!(self.custody.as_str(), "noncustodial") {
            return inv(format!(
                "economics.custody must be \"noncustodial\" (v1), got {}",
                self.custody
            ));
        }
        if !matches!(self.accounting_unit.as_str(), "ton") {
            return inv(format!(
                "economics.accounting_unit must be \"ton\" (v1), got {}",
                self.accounting_unit
            ));
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
            return inv(
                "economics.stake: require min_stake_sensitive >= min_stake_internal >= min_stake"
                    .into(),
            );
        }
        if self.stake.stake_cap < self.stake.min_stake {
            return inv("economics.stake.stake_cap must be >= min_stake".into());
        }

        // Slash split must sum to 1.0.
        let s = &self.slashing;
        let sum =
            s.slash_to_challenger + s.slash_to_redundancy + s.slash_to_burn + s.slash_to_treasury;
        if (sum - 1.0).abs() > 1e-9 {
            return inv(format!(
                "economics.slashing slash_to_* must sum to 1.0, got {sum}"
            ));
        }
        pct("slashing.slash_wrong_result_pct", s.slash_wrong_result_pct)?;
        pct("slashing.slash_cheat_pct", s.slash_cheat_pct)?;
        pct("slashing.slash_downtime_pct", s.slash_downtime_pct)?;
        pct("slashing.slash_equivocation_pct", s.slash_equivocation_pct)?;
        pct(
            "slashing.slash_failed_commitment_pct",
            s.slash_failed_commitment_pct,
        )?;
        pct("slashing.slash_to_challenger", s.slash_to_challenger)?;
        pct("slashing.slash_to_redundancy", s.slash_to_redundancy)?;
        pct("slashing.slash_to_burn", s.slash_to_burn)?;
        pct("slashing.slash_to_treasury", s.slash_to_treasury)?;

        // Fees.
        pct("fees.platform_fee_pct", self.fees.platform_fee_pct)?;
        pct(
            "fees.verification_surcharge_pct",
            self.fees.verification_surcharge_pct,
        )?;
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
        pct("ranking.exploration_rate", r.exploration_rate)?;

        // Quality weights must be non-negative.
        let q = &self.quality;
        if q.w_success < 0.0 || q.w_latency < 0.0 || q.w_throughput < 0.0 || q.w_completion < 0.0 {
            return inv("economics.quality weights must be >= 0".into());
        }

        // Reputation confidence priors must be non-negative.
        let rep = &self.reputation;
        if rep.prior_alpha < 0.0 || rep.prior_beta < 0.0 || rep.confidence_z < 0.0 {
            return inv(
                "economics.reputation priors (prior_alpha, prior_beta, confidence_z) must be >= 0"
                    .into(),
            );
        }

        // Selection ordering.
        let sel = &self.selection;
        if sel.checksum_min < 1 {
            return inv("economics.selection.checksum_min must be >= 1".into());
        }
        if !(sel.n_max >= sel.n_default
            && sel.n_default >= sel.n_public
            && sel.n_public >= sel.checksum_min)
        {
            return inv(
                "economics.selection: require n_max >= n_default >= n_public >= checksum_min"
                    .into(),
            );
        }

        // Records.
        pct("records.anchor_quorum_pct", self.records.anchor_quorum_pct)?;
        if self.records.epoch_secs == 0 {
            return inv("economics.records.epoch_secs must be >= 1".into());
        }

        // A fee recipient is required once on-chain fees can actually be charged
        // (the channel / on-chain rails). The noop and mock rails charge nothing.
        if self.enabled
            && matches!(
                self.settlement,
                SettlementRail::Channel | SettlementRail::Onchain
            )
        {
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
        assert_eq!(
            e.resolve_payment(DataClassCfg::Sensitive),
            PaymentMode::Free
        );
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
        assert_eq!(
            e.resolve_payment(DataClassCfg::Sensitive),
            PaymentMode::Paid
        );
    }

    #[test]
    fn explicit_pref_overrides_class() {
        let mut e = EconomicsConfig::default();
        e.enabled = true;
        e.fee_recipient = Some("EQ...treasury".into());
        e.settlement = SettlementRail::Channel;
        e.default_payment = PaymentPref::Free;
        assert_eq!(
            e.resolve_payment(DataClassCfg::Sensitive),
            PaymentMode::Free
        );
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

    #[test]
    fn default_network_is_testnet_and_safe() {
        let e = EconomicsConfig::default();
        assert_eq!(e.network, TonNetwork::Testnet);
        assert!(!e.mainnet_confirmed);
        // Testnet is never blocked; mainnet without confirm is.
        assert!(!e.mainnet_blocked());
        assert!(e.guard_mainnet().is_ok());
    }

    #[test]
    fn mainnet_requires_confirmation() {
        let mut e = EconomicsConfig::default();
        e.network = TonNetwork::Mainnet;
        assert!(e.mainnet_blocked());
        assert!(e.guard_mainnet().is_err());
        e.mainnet_confirmed = true;
        assert!(!e.mainnet_blocked());
        assert!(e.guard_mainnet().is_ok());
    }

    #[test]
    fn per_network_endpoints_resolve_with_defaults_and_overrides() {
        let mut e = EconomicsConfig::default();
        // Testnet defaults.
        assert_eq!(e.resolved_rpc(), "https://testnet.toncenter.com/api/v2/");
        assert_eq!(e.resolved_explorer(), "testnet.tonviewer.com");
        // Configure BOTH networks; switching flips endpoints/addresses.
        e.testnet.contracts.stake_vault = Some("kQtest".into());
        e.mainnet.contracts.stake_vault = Some("kQmain".into());
        e.mainnet.rpc = Some("https://my.mainnet.rpc/".into());
        assert_eq!(
            e.active_settings().contracts.stake_vault.as_deref(),
            Some("kQtest")
        );

        e.network = TonNetwork::Mainnet;
        e.mainnet_confirmed = true;
        assert_eq!(
            e.active_settings().contracts.stake_vault.as_deref(),
            Some("kQmain")
        );
        assert_eq!(e.resolved_rpc(), "https://my.mainnet.rpc/"); // override
        assert_eq!(e.resolved_explorer(), "tonviewer.com"); // mainnet default
    }

    #[test]
    fn reputation_priors_and_exploration_defaults_validate() {
        let e = EconomicsConfig::default();
        e.validate().unwrap();
        // Pessimistic-but-sane defaults.
        assert!(e.reputation.prior_beta >= e.reputation.prior_alpha);
        assert!(e.reputation.confidence_z > 0.0);
        // Exploration off by default (today's pure-exploitation ranking).
        assert_eq!(e.ranking.exploration_rate, 0.0);
        assert!(e.ranking.exploration_saturation > 0);
    }

    #[test]
    fn rejects_out_of_range_exploration_and_negative_priors() {
        let mut e = EconomicsConfig::default();
        e.ranking.exploration_rate = 1.5;
        assert!(e.validate().is_err());
        let mut e = EconomicsConfig::default();
        e.reputation.prior_beta = -1.0;
        assert!(e.validate().is_err());
    }

    #[test]
    fn mock_rail_needs_no_fee_recipient() {
        let mut e = EconomicsConfig::default();
        e.enabled = true;
        e.settlement = SettlementRail::Mock;
        e.fee_recipient = None;
        e.validate().unwrap();
    }
}
