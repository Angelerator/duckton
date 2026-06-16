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

use std::sync::Arc;

use p2p_config::{EconomicsConfig, SchedulerConfig, SettlementRail};

use crate::ton::GlobalParams;
use crate::traits::{RecordAnchor, Settlement, StakeRegistry};
use crate::types::{Amount, SettleError, WalletAddress};
use crate::ParamsSource;

const NANOTON_PER_TON: Amount = 1_000_000_000;

/// The settlement collaborators wired onto the live coordinator for one config.
///
/// This is the single construction site that closes the "test callers only" gap:
/// the production node resolves its money rail from `[economics]` here and wires
/// the resulting `settlement` + `record_anchor` (+ optional `params_source` for
/// the on-chain `GlobalParams` sync) onto the [`Coordinator`]. It DEFAULTS SAFELY
/// — a disabled chain or the `noop` rail yields the genuine no-op stack, and the
/// deterministic `mock` rail yields the in-memory doubles — so a node only ever
/// touches the chain when explicitly configured for the on-chain (`ton`) rail in
/// a `ton-live` build.
pub struct SettlementStack {
    pub settlement: Arc<dyn Settlement>,
    pub record_anchor: Arc<dyn RecordAnchor>,
    /// On-chain `GlobalParams` read seam for the startup + periodic policy sync.
    /// `None` for the noop/mock rails (no chain reads).
    pub params_source: Option<Arc<dyn ParamsSource>>,
    /// The stake registry the coordinator consults for the `require_staked_hosts`
    /// candidate gate and the paid `stake_factor` ranking term. `None` (default /
    /// free grid) ⇒ the coordinator behaves exactly as today (no stake gate, a
    /// `0.0` stake factor). The deterministic `mock` rail supplies an in-memory
    /// registry derived from `[economics.stake]`; the live `ton` rail's
    /// auto-constructed [`crate::ton::TonStakeRegistry`] is documented as remaining
    /// (it needs a `VaultConfig` cell matching the deployed vault + a node↔wallet
    /// binding source to resolve peer vaults). Embedders can always inject any
    /// `StakeRegistry` directly via `Node::with_wallet`.
    pub stake_registry: Option<Arc<dyn StakeRegistry>>,
    /// True only for a real value-moving on-chain rail (so callers can assert
    /// "no chain for the default/free grid").
    pub onchain: bool,
}

/// The genuine no-op stack: no escrow, no anchor, no chain reads (the free grid).
fn noop_stack() -> SettlementStack {
    SettlementStack {
        settlement: Arc::new(crate::noop::NoopSettlement),
        record_anchor: Arc::new(crate::noop::NoopRecordAnchor),
        params_source: None,
        stake_registry: None,
        onchain: false,
    }
}

/// Resolve the per-config settlement stack for the live coordinator path.
///
/// Defaults safely (see [`SettlementStack`]). The on-chain rail is built only in
/// a `ton-live` build; without that feature an `onchain`/`channel` rail falls
/// back to the no-op stack (a default build never attempts to broadcast).
pub fn resolve_settlement_stack(econ: &EconomicsConfig) -> Result<SettlementStack, SettleError> {
    // Mainnet safety gate first — never assemble a real rail on unconfirmed mainnet.
    econ.guard_mainnet().map_err(SettleError::Backend)?;
    if !econ.enabled {
        return Ok(noop_stack());
    }
    match econ.settlement {
        SettlementRail::Noop => Ok(noop_stack()),
        SettlementRail::Mock => Ok(SettlementStack {
            settlement: Arc::new(crate::mock::MockSettlement::new()),
            record_anchor: Arc::new(crate::mock::InMemoryRecordAnchor::new()),
            params_source: None,
            // The mock models the full paid path: a real (in-memory) stake
            // registry derived from `[economics.stake]`. Stakes are empty until an
            // operator/test sets them, so the gate/factor stay inert by default.
            stake_registry: Some(Arc::new(crate::mock::InMemoryStakeRegistry::from_config(
                &econ.stake,
            ))),
            onchain: true, // mock models the full paid path (no funds)
        }),
        SettlementRail::Onchain | SettlementRail::Channel => resolve_onchain_stack(econ),
    }
}

/// Build the live on-chain stack (only with `ton-live`). Without the feature the
/// on-chain rail safely degrades to the no-op stack so a default build can never
/// broadcast.
#[cfg(not(feature = "ton-live"))]
fn resolve_onchain_stack(_econ: &EconomicsConfig) -> Result<SettlementStack, SettleError> {
    Ok(noop_stack())
}

#[cfg(feature = "ton-live")]
fn resolve_onchain_stack(econ: &EconomicsConfig) -> Result<SettlementStack, SettleError> {
    use crate::ton::{
        build_escrow_terms, GlobalParamsClient, TonRecordAnchor, TonSettlement, ToncenterRpc,
    };

    let wiring = resolve_ton_wiring(econ)?;
    let mnemonic_path = econ
        .active_settings()
        .wallet
        .mnemonic_file
        .clone()
        .filter(|p| !p.trim().is_empty())
        .ok_or_else(|| {
            SettleError::Backend(
                "on-chain rail requires economics.<net>.wallet.mnemonic_file (a 0600 file)".into(),
            )
        })?;
    let mnemonic = std::fs::read_to_string(&mnemonic_path)
        .map_err(|e| {
            SettleError::Backend(format!("cannot read mnemonic_file {mnemonic_path}: {e}"))
        })?
        .trim()
        .to_string();

    // The signer's wallet is the requester/arbiter/treasury for the turnkey node;
    // a fresh ToncenterRpc per consumer (each derives the same wallet) since the
    // RPC owns its signer by value.
    let mk_rpc = || {
        ToncenterRpc::new(
            &wiring.rpc_endpoint,
            wiring.api_key.clone(),
            &wiring.network,
            &mnemonic,
        )
    };
    let wallet = mk_rpc()?.wallet_address();
    // FEE-RECIPIENT precedence (the admin treasury is chain-authoritative):
    //   * The coordinator binds the per-job escrow `treasury` from the SYNCED
    //     on-chain `GlobalParams.fee_recipient` (passed into `open_escrow_with_terms`).
    //   * A local `economics.fee_recipient`, when configured, is ONLY an optional
    //     cross-check: the `ton` rail rejects a settle whose chain value disagrees
    //     with this local override (`SettleError::TreasuryMismatch`) — it can never
    //     silently override the admin treasury.
    //   * When NOT configured (None), no local override is set and the chain value
    //     is used verbatim. A placeholder (the signer wallet) seeds only the unused
    //     shared terms cell + the no-params-source fallback.
    let local_override = econ
        .fee_recipient
        .as_deref()
        .and_then(|s| WalletAddress::from_any_str(s).ok());
    let placeholder_treasury = local_override.unwrap_or(wallet);

    // Escrow code from the compiled artifact embedded at build time, so the
    // per-job escrow address derivation matches the deployed contract.
    let escrow_code = embedded_job_escrow_code()?;
    // FEE ENFORCEMENT (φ): the admin platform-fee rate bound into each per-job
    // escrow at open. Sourced from `[economics].fees` (the same single source the
    // deployed `GlobalParams` and the coordinator's split derive from), so the
    // bound φ, the coordinator's computed fee, and the on-chain GlobalParams agree.
    let platform_fee_bps =
        (econ.fees.platform_fee_pct * 10_000.0).round().clamp(0.0, 65_535.0) as u16;
    let mut settlement = TonSettlement::with_escrow_code(
        mk_rpc()?,
        escrow_code,
        // Placeholder shared terms cell (a fresh per-job `EscrowTerms` is rebuilt
        // inside `open_escrow_with_terms`): unbound expected-hash + candidates-hash,
        // params version 0, and the bound platform-fee rate φ.
        build_escrow_terms(&placeholder_treasury, &[0u8; 32], &[0u8; 32], 0, platform_fee_bps),
        wallet,
    )
    .with_requester(wallet)
    .with_platform_fee_bps(platform_fee_bps)
    // ~0.05 TON deploy headroom so the per-job escrow can pay its own settle-time
    // action (forward) fees (the locked B is unaffected). Mirrors the Acton
    // deploy script's `escrowAmount + buffer` funding.
    .with_deploy_gas_buffer(50_000_000)
    // ~0.05 TON attached to the settle/refund message so the escrow's compute
    // phase has gas (a 0-value internal message aborts before compute on TON).
    .with_settle_gas(50_000_000);
    let window = (econ.slashing.challenge_window_secs.min(u32::MAX as u64) as u32).max(600);
    settlement = settlement.with_escrow_window(window);
    // Only set a local treasury when `economics.fee_recipient` is explicitly
    // configured — then it is the cross-check the `ton` rail enforces against the
    // chain value. Unset ⇒ the chain `GlobalParams.fee_recipient` is used verbatim.
    if let Some(local) = local_override {
        settlement = settlement.with_treasury(local);
    }

    let anchor: Arc<dyn RecordAnchor> = match wiring.record_anchor {
        Some(addr) => Arc::new(TonRecordAnchor::new(mk_rpc()?, addr)),
        None => Arc::new(crate::noop::NoopRecordAnchor),
    };
    let params_source: Option<Arc<dyn ParamsSource>> = match wiring.global_params {
        Some(addr) => Some(Arc::new(GlobalParamsClient::new(mk_rpc()?, addr))),
        None => None,
    };

    // REMAINING (documented): an auto-constructed `TonStakeRegistry` for the live
    // gate/factor needs (a) a `VaultConfig` cell builder matching the deployed
    // `StakeVault` config and (b) a node↔wallet binding source (gossip or a
    // persistent store) to resolve each peer's per-node vault address. Until those
    // land, the live rail leaves `stake_registry` unset (the coordinator then
    // behaves as today); embedders with bindings can inject a `TonStakeRegistry`
    // directly via `Node::with_wallet`.
    Ok(SettlementStack {
        settlement: Arc::new(settlement),
        record_anchor: anchor,
        params_source,
        stake_registry: None,
        onchain: true,
    })
}

/// The compiled `JobEscrow` code cell, embedded from `ton/build/JobEscrow.json`
/// at build time (only needed by the live rail).
#[cfg(feature = "ton-live")]
fn embedded_job_escrow_code() -> Result<crate::cell::Cell, SettleError> {
    const ARTIFACT: &str = include_str!("../../../ton/build/JobEscrow.json");
    let v: serde_json::Value = serde_json::from_str(ARTIFACT)
        .map_err(|e| SettleError::Backend(format!("JobEscrow.json parse: {e}")))?;
    let code_b64 = v
        .get("code_boc64")
        .and_then(|s| s.as_str())
        .ok_or_else(|| SettleError::Backend("JobEscrow.json: missing code_boc64".into()))?;
    crate::ton::escrow_code_from_boc_base64(code_b64)
}

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
    s.as_deref()
        .and_then(|v| WalletAddress::from_any_str(v).ok())
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
        e.testnet.contracts.stake_vault =
            Some("0:1111111111111111111111111111111111111111111111111111111111111111".into());
        e.testnet.contracts.global_params =
            Some("0:2222222222222222222222222222222222222222222222222222222222222222".into());
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
        e.mainnet.contracts.stake_vault =
            Some("0:3333333333333333333333333333333333333333333333333333333333333333".into());
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
        p.validate()
            .expect("derived params must satisfy the on-chain bounds");
        assert_eq!(p.platform_fee_bps, 1500); // 0.15 -> 1500 bps (canonical φ)
        assert_eq!(p.participation_commission_bps, 500); // 0.05 -> 500 bps (canonical κ)
        assert_eq!(p.split_challenger_bps, 4000);
        assert_eq!(p.min_stake_internal, 100 * NANOTON_PER_TON);
        // Resilience fields default-derived (from SchedulerConfig::default()).
        assert_eq!(p.slash_failed_commitment_bps, 1000); // 0.1 -> 1000 bps
        assert_eq!(p.attempt_deadline_ms, 60_000);
        assert_eq!(p.progress_interval_ms, 2_000);
        assert_eq!(p.progress_stall_mult, 5);
    }

    #[test]
    fn settlement_stack_defaults_safely() {
        // Disabled chain ⇒ genuine no-op stack (no escrow/anchor/params reads).
        let e = EconomicsConfig::default();
        let s = resolve_settlement_stack(&e).unwrap();
        assert!(!s.onchain, "default/free grid is not on-chain");
        assert!(s.params_source.is_none());
        assert!(
            s.stake_registry.is_none(),
            "free grid wires no stake registry"
        );
        assert!(!s.settlement.is_onchain());

        // Explicit noop rail ⇒ same no-op stack.
        let mut e = EconomicsConfig::default();
        e.enabled = true;
        e.settlement = SettlementRail::Noop;
        e.validate().unwrap();
        let s = resolve_settlement_stack(&e).unwrap();
        assert!(!s.onchain);
        assert!(s.params_source.is_none());
    }

    #[test]
    fn settlement_stack_mock_models_paid_path() {
        let mut e = EconomicsConfig::default();
        e.enabled = true;
        e.settlement = SettlementRail::Mock;
        e.validate().unwrap();
        let s = resolve_settlement_stack(&e).unwrap();
        assert!(s.onchain, "mock models the paid path");
        assert!(s.settlement.is_onchain());
        // Mock reads no on-chain GlobalParams (no params source).
        assert!(s.params_source.is_none());
        // Mock supplies an in-memory stake registry (empty stakes ⇒ inert until
        // set), so a mock paid node models the full stake-aware path.
        let reg = s.stake_registry.expect("mock supplies a stake registry");
        assert_eq!(reg.stake_of(&p2p_proto::NodeId("b3:x".into())), 0);
    }

    #[test]
    fn settlement_stack_mainnet_unconfirmed_is_guarded() {
        let mut e = EconomicsConfig::default();
        e.enabled = true;
        e.network = p2p_config::TonNetwork::Mainnet;
        e.settlement = SettlementRail::Onchain;
        // The mainnet guard must refuse to assemble any rail when unconfirmed.
        assert!(resolve_settlement_stack(&e).is_err());
    }

    #[cfg(not(feature = "ton-live"))]
    #[test]
    fn settlement_stack_onchain_without_ton_live_falls_back_to_noop() {
        let mut e = EconomicsConfig::default();
        e.enabled = true;
        e.settlement = SettlementRail::Onchain;
        e.fee_recipient = Some("0:00".into());
        e.validate().unwrap();
        // Default (non-ton-live) build can never broadcast: degrade to no-op.
        let s = resolve_settlement_stack(&e).unwrap();
        assert!(
            !s.onchain,
            "on-chain rail degrades to no-op without ton-live"
        );
        assert!(s.params_source.is_none());
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
