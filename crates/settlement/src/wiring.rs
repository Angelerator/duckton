//! Live TON client wiring from `[economics]` (BLOCKCHAIN_ECONOMICS §12).
//!
//! This is the construction site that the deferred follow-up called for: it
//! reads the **per-network** endpoint/addresses + API-key reference from the
//! resolved config and refuses to act on mainnet without explicit confirmation.
//! Concretely it sources:
//!   * the RPC endpoint via [`EconomicsConfig::resolved_rpc`],
//!   * the toncenter API key from `active_settings().api_key_file` (a `0600`
//!     file path — never an inline secret),
//!   * the deployed contract addresses from `active_settings().contracts.*`,
//! and calls [`EconomicsConfig::guard_mainnet`] **before** anything that could
//! move real funds, so flipping `network = mainnet` without
//! `mainnet_confirmed = true` fails fast.
//!
//! Resolution is pure (no network), so it is unit-tested in the default build;
//! the actual HTTP hop lives behind the `ton-live` feature.

use p2p_config::{EconomicsConfig, SchedulerConfig};

use crate::ton::GlobalParams;
use crate::types::{Amount, SettleError, WalletAddress};

const NANOTON_PER_TON: Amount = 1_000_000_000;

/// Per-network TON client wiring resolved from `[economics]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TonWiring {
    /// Effective toncenter RPC endpoint for the active network.
    pub rpc_endpoint: String,
    /// Toncenter API key read from `api_key_file` (if configured + readable).
    pub api_key: Option<String>,
    /// Active network name (`"testnet"` | `"mainnet"`).
    pub network: String,
    /// Deployed `StakeVault` address (parsed from `contracts.stake_vault`).
    pub stake_vault: Option<WalletAddress>,
    /// Deployed `JobEscrow` template address.
    pub job_escrow: Option<WalletAddress>,
    /// Deployed `RecordAnchor` address.
    pub record_anchor: Option<WalletAddress>,
    /// Deployed `GlobalParams` address (stable; safe to pin).
    pub global_params: Option<WalletAddress>,
}

/// Read a `0600` secret file (e.g. the toncenter API key), trimmed. `None` path
/// ⇒ `Ok(None)`; a configured-but-unreadable path ⇒ a clear error.
fn read_optional_secret(path: Option<&str>) -> Result<Option<String>, SettleError> {
    match path {
        None => Ok(None),
        Some(p) if p.trim().is_empty() => Ok(None),
        Some(p) => {
            let raw = std::fs::read_to_string(p)
                .map_err(|e| SettleError::Backend(format!("cannot read api_key_file {p}: {e}")))?;
            let key = raw.trim().to_string();
            Ok(if key.is_empty() { None } else { Some(key) })
        }
    }
}

/// Best-effort parse of a configured contract address. Accepts BOTH the raw
/// `"workchain:hex"` form AND the user-facing base64 (`EQ…`/`UQ…`/`kQ…`/`0Q…`)
/// form the explorer / faucet / Acton print, normalizing the latter offline
/// (CRC16-checked) so the deployed addresses resolve without a network hop.
fn parse_addr(s: &Option<String>) -> Option<WalletAddress> {
    s.as_deref().and_then(|v| WalletAddress::from_any_str(v).ok())
}

/// Resolve the live TON client wiring from `[economics]`, guarding mainnet.
///
/// Returns an error when the active network is **mainnet but not confirmed** (so
/// no caller can transact real TON by accident), or when a configured
/// `api_key_file` cannot be read.
pub fn resolve_ton_wiring(econ: &EconomicsConfig) -> Result<TonWiring, SettleError> {
    // Mainnet safety gate FIRST — before any endpoint/secret is even assembled.
    econ.guard_mainnet().map_err(SettleError::Backend)?;

    let settings = econ.active_settings();
    Ok(TonWiring {
        rpc_endpoint: econ.resolved_rpc(),
        api_key: read_optional_secret(settings.api_key_file.as_deref())?,
        network: econ.network.as_str().to_string(),
        stake_vault: parse_addr(&settings.contracts.stake_vault),
        job_escrow: parse_addr(&settings.contracts.job_escrow),
        record_anchor: parse_addr(&settings.contracts.record_anchor),
        global_params: parse_addr(&settings.contracts.global_params),
    })
}

impl GlobalParams {
    /// Derive the on-chain economic params from `[economics]` (the single source
    /// of truth) using **default** resilience gating. Prefer
    /// [`GlobalParams::from_config_parts`] when the node's `[scheduler]` is
    /// available so the resilience fairness fields reflect the real config.
    pub fn from_economics(econ: &EconomicsConfig) -> Self {
        Self::from_config_parts(econ, &SchedulerConfig::default())
    }

    /// Derive the full on-chain param set from `[economics]` + `[scheduler]`
    /// (the single source of truth). Whole-TON stake amounts become nanoton;
    /// percentage/fraction knobs become basis points; the resilience fairness
    /// gating (failed-commitment slash, attempt deadline, progress interval +
    /// stall multiplier) is sourced from `[scheduler]` and
    /// `[economics.slashing]`. New escrow/stake instances are parameterized from
    /// this, identically to what the admin pushes on-chain via `update_params`.
    pub fn from_config_parts(econ: &EconomicsConfig, sched: &SchedulerConfig) -> Self {
        let bps = |x: f64| (x * 10_000.0).round().clamp(0.0, 65_535.0) as u16;
        let ton = |whole: u64| (whole as Amount) * NANOTON_PER_TON;
        let s = &econ.slashing;
        let f = &econ.fees;
        let st = &econ.stake;
        let sel = &econ.selection;
        let r = &econ.ranking;
        Self {
            platform_fee_bps: bps(f.platform_fee_pct),
            surcharge_bps: bps(f.verification_surcharge_pct),
            participation_commission_bps: bps(f.participation_commission_frac),
            slash_wrong_bps: bps(s.slash_wrong_result_pct),
            slash_cheat_bps: bps(s.slash_cheat_pct),
            slash_downtime_bps: bps(s.slash_downtime_pct),
            slash_equivocation_bps: bps(s.slash_equivocation_pct),
            split_challenger_bps: bps(s.slash_to_challenger),
            split_redundancy_bps: bps(s.slash_to_redundancy),
            split_burn_bps: bps(s.slash_to_burn),
            split_treasury_bps: bps(s.slash_to_treasury),
            min_stake: ton(st.min_stake),
            min_stake_internal: ton(st.min_stake_internal),
            min_stake_sensitive: ton(st.min_stake_sensitive),
            stake_cap: ton(st.stake_cap),
            unbonding_secs: st.unbonding_secs.min(u32::MAX as u64) as u32,
            challenge_window_secs: s.challenge_window_secs.min(u32::MAX as u64) as u32,
            n_public: sel.n_public.min(u8::MAX as usize) as u8,
            n_default: sel.n_default.min(u8::MAX as usize) as u8,
            n_max: sel.n_max.min(u8::MAX as usize) as u8,
            // economics has no separate quorum; the checksum minimum is the
            // agreement floor, so it doubles as the on-chain quorum.
            quorum: sel.checksum_min.min(u8::MAX as usize) as u8,
            checksum_min: sel.checksum_min.min(u8::MAX as usize) as u8,
            w_quality_bps: bps(r.w_quality),
            w_stake_bps: bps(r.w_stake),
            w_price_bps: bps(r.w_price),
            // Resilience / fairness gating: the failed-commitment slash is an
            // economics value; the deadline/stall knobs come from [scheduler].
            slash_failed_commitment_bps: bps(s.slash_failed_commitment_pct),
            attempt_deadline_ms: sched.attempt_deadline_ms.min(u32::MAX as u64) as u32,
            progress_interval_ms: sched.progress_interval_ms.max(1).min(u32::MAX as u64) as u32,
            progress_stall_mult: sched.progress_stall_multiplier.max(1).min(u8::MAX as u32) as u8,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p2p_config::{SettlementRail, TonNetwork};

    #[test]
    fn resolves_testnet_endpoint_and_addresses() {
        let mut e = EconomicsConfig::default();
        e.testnet.contracts.stake_vault = Some("0:1111111111111111111111111111111111111111111111111111111111111111".into());
        e.testnet.contracts.global_params = Some("0:2222222222222222222222222222222222222222222222222222222222222222".into());
        let w = resolve_ton_wiring(&e).unwrap();
        assert_eq!(w.network, "testnet");
        assert_eq!(w.rpc_endpoint, "https://testnet.toncenter.com/api/v2/");
        assert!(w.stake_vault.is_some());
        assert!(w.global_params.is_some());
        assert!(w.job_escrow.is_none());
    }

    #[test]
    fn resolves_user_friendly_base64_addresses() {
        // The deployed addresses are registered in the user-facing `kQ…` base64
        // form (what the explorer / Acton print). The wiring must normalize them
        // offline rather than dropping them (the former gap).
        let mut e = EconomicsConfig::default();
        e.testnet.contracts.stake_vault =
            Some("kQDBwfWwUy7EXuukEb5QCsrUme0Ri2XndhuPs0Lozb5TrrXx".into());
        let w = resolve_ton_wiring(&e).unwrap();
        let vault = w.stake_vault.expect("kQ… vault address must resolve");
        assert_eq!(vault.workchain, 0);
        assert_eq!(
            vault.to_raw_string(),
            "0:c1c1f5b0532ec45eeba411be500acad499ed118b65e7761b8fb342e8cdbe53ae"
        );
    }

    #[test]
    fn mainnet_unconfirmed_is_guarded() {
        let mut e = EconomicsConfig::default();
        e.network = TonNetwork::Mainnet;
        // Not confirmed → wiring refuses to resolve (no real-fund footgun).
        assert!(resolve_ton_wiring(&e).is_err());
        // Confirmed → resolves to the mainnet endpoint + addresses.
        e.mainnet_confirmed = true;
        e.mainnet.rpc = Some("https://my.mainnet.rpc/".into());
        e.mainnet.contracts.stake_vault = Some("0:3333333333333333333333333333333333333333333333333333333333333333".into());
        let w = resolve_ton_wiring(&e).unwrap();
        assert_eq!(w.network, "mainnet");
        assert_eq!(w.rpc_endpoint, "https://my.mainnet.rpc/");
        assert!(w.stake_vault.is_some());
    }

    #[test]
    fn api_key_file_is_read_when_present() {
        let mut path = std::env::temp_dir();
        path.push(format!("p2p_apikey_{}.txt", std::process::id()));
        std::fs::write(&path, "  secret-key-123\n").unwrap();
        let mut e = EconomicsConfig::default();
        e.testnet.api_key_file = Some(path.display().to_string());
        let w = resolve_ton_wiring(&e).unwrap();
        assert_eq!(w.api_key.as_deref(), Some("secret-key-123"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn global_params_derived_from_economics_are_valid() {
        // A validated EconomicsConfig must produce on-chain params that pass the
        // mirrored §12 invariants.
        let mut e = EconomicsConfig::default();
        e.enabled = true;
        e.settlement = SettlementRail::Onchain;
        e.fee_recipient = Some("0:00".into());
        e.validate().unwrap();
        let p = GlobalParams::from_economics(&e);
        p.validate().expect("derived params must satisfy the on-chain bounds");
        assert_eq!(p.platform_fee_bps, 200); // 0.02 -> 200 bps
        assert_eq!(p.split_challenger_bps, 4000);
        assert_eq!(p.min_stake_internal, 100 * NANOTON_PER_TON);
        // Resilience fields default-derived (from SchedulerConfig::default()).
        assert_eq!(p.slash_failed_commitment_bps, 1000); // 0.1 -> 1000 bps
        assert_eq!(p.attempt_deadline_ms, 60_000);
        assert_eq!(p.progress_interval_ms, 2_000);
        assert_eq!(p.progress_stall_mult, 5);
    }

    #[test]
    fn from_config_parts_sources_resilience_gating_from_scheduler() {
        use p2p_config::SchedulerConfig;
        let mut e = EconomicsConfig::default();
        e.slashing.slash_failed_commitment_pct = 0.25;
        let mut sched = SchedulerConfig::default();
        sched.attempt_deadline_ms = 90_000;
        sched.progress_interval_ms = 3_000;
        sched.progress_stall_multiplier = 4;
        let p = GlobalParams::from_config_parts(&e, &sched);
        p.validate().expect("derived params valid");
        // The on-chain fairness gating reflects the node's [scheduler] config.
        assert_eq!(p.slash_failed_commitment_bps, 2500);
        assert_eq!(p.attempt_deadline_ms, 90_000);
        assert_eq!(p.progress_interval_ms, 3_000);
        assert_eq!(p.progress_stall_mult, 4);
    }
}
