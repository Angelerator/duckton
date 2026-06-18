//! `duckton` — the loadable DuckDB C-API extension surface (architecture §12).
//!
//! Duckton is built as a loadable extension against DuckDB's **stable C extension
//! API** (so it loads via `LOAD 'duckton'` without linking the whole engine). The
//! published flow is `INSTALL duckton FROM community; LOAD duckton;`. It exposes
//! table functions (the `p2p_*` SQL surface) wired to the workspace crates:
//!
//!  * `p2p_info()`   → protocol/version/build metadata (from `p2p-proto`).
//!  * `p2p_peers()`  → the bootstrap/seed peers from the resolved config
//!                     (`p2p-config`, honoring the `P2P_CONFIG` env var).
//!
//! The full distributed `p2p_query` / `p2p_share` / `p2p_join` surface drives the
//! async coordinator/worker in `p2p-node`; that path is exercised by the Rust
//! scenario suite (it needs live peers, which a single in-process `LOAD` cannot
//! provide). See `docs/ARCHITECTURE.md` and the scenario suite.

use std::collections::BTreeMap;
use std::error::Error;
use std::ffi::CString;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use duckdb::core::{DataChunkHandle, Inserter, LogicalTypeHandle, LogicalTypeId};
use duckdb::types::ValueRef;
use duckdb::vtab::{BindInfo, InitInfo, TableFunctionInfo, VTab};
use duckdb::{duckdb_entrypoint_c_api, Connection, Result};

use p2p_config::{
    BlockKind, BlocklistStore, ConfigStore, DataClassCfg, PaymentPref, PreferMode, QueryOverrides,
    SettingRow, VerifyModeCfg,
};
use p2p_node::{EngineError, ExecLease, Node, QueryEngine};
use p2p_proto::{ResultSet, Value as PValue};

/// Rows materialized at bind time; emitted in one chunk.
#[repr(C)]
struct Rows2 {
    rows: Vec<(String, String)>,
}

#[repr(C)]
struct OnceInit {
    done: AtomicBool,
}

/// `p2p_info()` → (key VARCHAR, value VARCHAR).
struct InfoVTab;

impl VTab for InfoVTab {
    type InitData = OnceInit;
    type BindData = Rows2;

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn Error>> {
        bind.add_result_column("key", LogicalTypeHandle::from(LogicalTypeId::Varchar));
        bind.add_result_column("value", LogicalTypeHandle::from(LogicalTypeId::Varchar));
        let rows = vec![
            (
                "protocol_name".to_string(),
                p2p_proto::PROTOCOL_NAME.to_string(),
            ),
            (
                "protocol_version".to_string(),
                p2p_proto::PROTOCOL_VERSION.to_string(),
            ),
            (
                "min_supported_version".to_string(),
                p2p_proto::MIN_SUPPORTED_VERSION.to_string(),
            ),
            (
                "schema_version".to_string(),
                p2p_proto::SCHEMA_VERSION.to_string(),
            ),
            (
                "extension_version".to_string(),
                env!("CARGO_PKG_VERSION").to_string(),
            ),
            (
                "alpn".to_string(),
                String::from_utf8_lossy(&p2p_proto::current_alpn()).to_string(),
            ),
        ];
        Ok(Rows2 { rows })
    }

    fn init(_: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        Ok(OnceInit {
            done: AtomicBool::new(false),
        })
    }

    fn func(
        func: &TableFunctionInfo<Self>,
        output: &mut DataChunkHandle,
    ) -> Result<(), Box<dyn Error>> {
        let init = func.get_init_data();
        let bind = func.get_bind_data();
        if init.done.swap(true, Ordering::Relaxed) {
            output.set_len(0);
            return Ok(());
        }
        emit_two_columns(output, &bind.rows)?;
        Ok(())
    }

    fn parameters() -> Option<Vec<LogicalTypeHandle>> {
        Some(vec![])
    }
}

/// `p2p_peers()` → (kind VARCHAR, value VARCHAR) describing configured seeds.
struct PeersVTab;

impl VTab for PeersVTab {
    type InitData = OnceInit;
    type BindData = Rows2;

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn Error>> {
        bind.add_result_column("kind", LogicalTypeHandle::from(LogicalTypeId::Varchar));
        bind.add_result_column("value", LogicalTypeHandle::from(LogicalTypeId::Varchar));

        // Resolve config (defaults <- file via P2P_CONFIG <- env). On error,
        // surface a single diagnostic row instead of failing the LOAD.
        let rows = match p2p_config::GridConfig::load(None) {
            Ok(cfg) => {
                let mut rows = vec![
                    (
                        "discovery_mode".to_string(),
                        format!("{:?}", cfg.discovery.mode),
                    ),
                    (
                        "candidate_sample_size".to_string(),
                        cfg.discovery.candidate_sample_size.to_string(),
                    ),
                ];
                for seed in &cfg.discovery.bootstrap {
                    rows.push(("bootstrap".to_string(), seed.clone()));
                }
                rows
            }
            Err(e) => vec![("config_error".to_string(), e.to_string())],
        };
        Ok(Rows2 { rows })
    }

    fn init(_: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        Ok(OnceInit {
            done: AtomicBool::new(false),
        })
    }

    fn func(
        func: &TableFunctionInfo<Self>,
        output: &mut DataChunkHandle,
    ) -> Result<(), Box<dyn Error>> {
        let init = func.get_init_data();
        let bind = func.get_bind_data();
        if init.done.swap(true, Ordering::Relaxed) {
            output.set_len(0);
            return Ok(());
        }
        emit_two_columns(output, &bind.rows)?;
        Ok(())
    }

    fn parameters() -> Option<Vec<LogicalTypeHandle>> {
        Some(vec![])
    }
}

/// Emit `rows` as two VARCHAR columns into a single output chunk.
fn emit_two_columns(
    output: &mut DataChunkHandle,
    rows: &[(String, String)],
) -> Result<(), Box<dyn Error>> {
    {
        let col0 = output.flat_vector(0);
        for (i, (k, _)) in rows.iter().enumerate() {
            col0.insert(i, CString::new(k.as_str())?);
        }
    }
    {
        let col1 = output.flat_vector(1);
        for (i, (_, v)) in rows.iter().enumerate() {
            col1.insert(i, CString::new(v.as_str())?);
        }
    }
    output.set_len(rows.len());
    Ok(())
}

// ===========================================================================
// SQL admin / configuration surface (architecture §12).
//
// A non-technical user manages everything — economics, network mode, wallet
// references, pricing, bidding, stake, fees, trust/selection, contracts —
// entirely via SQL `CALL`s. All logic + validation + persistence + secret
// redaction lives in the typed `p2p-config` `ConfigStore`; these table
// functions are a thin binding. Zero-config defaults apply until a user sets
// anything. Errors surface as friendly messages (the `ConfigStore` never
// panics on bad input).
// ===========================================================================

/// Rows materialized at bind time as (group, key, value); emitted in one chunk.
#[repr(C)]
struct Rows3 {
    rows: Vec<[String; 3]>,
}

fn boxed(e: impl std::fmt::Display) -> Box<dyn Error> {
    e.to_string().into()
}

fn row3(r: SettingRow) -> [String; 3] {
    [r.group, r.key, r.value]
}

fn add_three_columns(bind: &BindInfo, c0: &str, c1: &str, c2: &str) {
    bind.add_result_column(c0, LogicalTypeHandle::from(LogicalTypeId::Varchar));
    bind.add_result_column(c1, LogicalTypeHandle::from(LogicalTypeId::Varchar));
    bind.add_result_column(c2, LogicalTypeHandle::from(LogicalTypeId::Varchar));
}

fn emit_three_columns(
    output: &mut DataChunkHandle,
    rows: &[[String; 3]],
) -> Result<(), Box<dyn Error>> {
    for col in 0..3 {
        let vector = output.flat_vector(col);
        for (i, r) in rows.iter().enumerate() {
            vector.insert(i, CString::new(r[col].as_str())?);
        }
    }
    output.set_len(rows.len());
    Ok(())
}

/// Shared `func` body for every (group, key, value) table function: emit the
/// bind-time rows exactly once.
fn emit_rows3<T>(
    func: &TableFunctionInfo<T>,
    output: &mut DataChunkHandle,
) -> Result<(), Box<dyn Error>>
where
    T: VTab<InitData = OnceInit, BindData = Rows3>,
{
    let init = func.get_init_data();
    let bind = func.get_bind_data();
    if init.done.swap(true, Ordering::Relaxed) {
        output.set_len(0);
        return Ok(());
    }
    emit_three_columns(output, &bind.rows)
}

fn once_init(_: &InitInfo) -> Result<OnceInit, Box<dyn Error>> {
    Ok(OnceInit {
        done: AtomicBool::new(false),
    })
}

/// Collect the named parameters that were actually supplied into a string map.
fn collect_named(bind: &BindInfo, names: &[&str]) -> BTreeMap<String, String> {
    let mut params = BTreeMap::new();
    for name in names {
        if let Some(v) = bind.get_named_parameter(name) {
            params.insert((*name).to_string(), v.to_string());
        }
    }
    params
}

/// Generate a grouped, friendly setter table function (e.g. `p2p_economics`).
/// On success it returns the resulting node `status()` rows so the caller sees
/// the effect (prominently, the active network).
macro_rules! group_setter {
    ($vtab:ident, $group:literal, [$(($pname:literal, $ptype:ident)),* $(,)?]) => {
        struct $vtab;
        impl VTab for $vtab {
            type InitData = OnceInit;
            type BindData = Rows3;

            fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn Error>> {
                add_three_columns(bind, "group", "key", "value");
                let params = collect_named(bind, &[$($pname),*]);
                let store = ConfigStore::open();
                let cfg = store.apply_group($group, &params).map_err(boxed)?;
                let rows = p2p_config::status_rows(&cfg).into_iter().map(row3).collect();
                Ok(Rows3 { rows })
            }

            fn init(info: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
                once_init(info)
            }

            fn func(func: &TableFunctionInfo<Self>, output: &mut DataChunkHandle) -> Result<(), Box<dyn Error>> {
                emit_rows3(func, output)
            }

            fn named_parameters() -> Option<Vec<(String, LogicalTypeHandle)>> {
                Some(vec![
                    $(($pname.to_string(), LogicalTypeHandle::from(LogicalTypeId::$ptype))),*
                ])
            }
        }
    };
}

group_setter!(
    EconomicsVTab,
    "economics",
    [
        ("enabled", Boolean),
        ("settlement", Varchar),
        ("network", Varchar),
        ("confirm", Boolean),
        ("fee_recipient", Varchar),
        ("default_payment", Varchar),
    ]
);
group_setter!(
    PricingVTab,
    "pricing",
    [("unit_price", Bigint), ("max_bid", Bigint),]
);
group_setter!(
    BiddingVTab,
    "bidding",
    [
        ("w_quality", Double),
        ("w_stake", Double),
        ("w_price", Double),
        ("stake_reliability_floor", Double),
    ]
);
group_setter!(
    SelectionVTab,
    "selection",
    [
        ("replicas", Bigint),
        ("quorum", Bigint),
        ("checksum_min", Bigint),
        ("n_public", Bigint),
        ("n_default", Bigint),
        ("n_max", Bigint),
    ]
);
group_setter!(
    FeesVTab,
    "fees",
    [
        ("platform_fee_pct", Double),
        ("participation_commission_frac", Double),
        ("verification_surcharge_pct", Double),
        ("bonus_aggressiveness", Double),
    ]
);
group_setter!(
    TrustVTab,
    "trust",
    [
        ("min_trust", Double),
        ("min_attest", Varchar),
        ("min_attestation", Varchar),
    ]
);
group_setter!(
    PlannerVTab,
    "planner",
    [
        ("prefer", Varchar),
        ("local_execution", Boolean),
        ("local_execution_enabled", Boolean),
        ("enabled", Boolean),
    ]
);
group_setter!(
    ContractsVTab,
    "contracts",
    [
        ("stake_vault", Varchar),
        ("job_escrow", Varchar),
        ("record_anchor", Varchar),
        ("global_params", Varchar),
    ]
);
group_setter!(
    WalletVTab,
    "wallet",
    [
        ("rpc", Varchar),
        ("address", Varchar),
        ("mnemonic_file", Varchar),
        ("api_key_file", Varchar),
        ("mnemonic", Varchar),
        ("api_key", Varchar),
    ]
);

/// `SELECT * FROM p2p_config()` / `p2p_settings()` — effective settings, grouped
/// and human-readable, with secrets redacted.
struct ConfigInspectVTab;
impl VTab for ConfigInspectVTab {
    type InitData = OnceInit;
    type BindData = Rows3;

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn Error>> {
        add_three_columns(bind, "group", "key", "value");
        let rows = ConfigStore::open()
            .settings()
            .map_err(boxed)?
            .into_iter()
            .map(row3)
            .collect();
        Ok(Rows3 { rows })
    }
    fn init(info: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        once_init(info)
    }
    fn func(
        func: &TableFunctionInfo<Self>,
        output: &mut DataChunkHandle,
    ) -> Result<(), Box<dyn Error>> {
        emit_rows3(func, output)
    }
    fn parameters() -> Option<Vec<LogicalTypeHandle>> {
        Some(vec![])
    }
}

/// `SELECT * FROM p2p_status()` — node/wallet/network/economics state summary.
struct StatusVTab;
impl VTab for StatusVTab {
    type InitData = OnceInit;
    type BindData = Rows3;

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn Error>> {
        add_three_columns(bind, "group", "key", "value");
        let rows = ConfigStore::open()
            .status()
            .map_err(boxed)?
            .into_iter()
            .map(row3)
            .collect();
        Ok(Rows3 { rows })
    }
    fn init(info: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        once_init(info)
    }
    fn func(
        func: &TableFunctionInfo<Self>,
        output: &mut DataChunkHandle,
    ) -> Result<(), Box<dyn Error>> {
        emit_rows3(func, output)
    }
    fn parameters() -> Option<Vec<LogicalTypeHandle>> {
        Some(vec![])
    }
}

/// `CALL p2p_set('dotted.key.path', value)` — generic escape hatch to any key.
struct SetVTab;
impl VTab for SetVTab {
    type InitData = OnceInit;
    type BindData = Rows3;

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn Error>> {
        add_three_columns(bind, "group", "key", "value");
        let key = bind.get_parameter(0).to_string();
        let value = bind.get_parameter(1).to_string();
        let cfg = ConfigStore::open().set_kv(&key, &value).map_err(boxed)?;
        let rows = p2p_config::status_rows(&cfg)
            .into_iter()
            .map(row3)
            .collect();
        Ok(Rows3 { rows })
    }
    fn init(info: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        once_init(info)
    }
    fn func(
        func: &TableFunctionInfo<Self>,
        output: &mut DataChunkHandle,
    ) -> Result<(), Box<dyn Error>> {
        emit_rows3(func, output)
    }
    fn parameters() -> Option<Vec<LogicalTypeHandle>> {
        Some(vec![
            LogicalTypeHandle::from(LogicalTypeId::Varchar),
            LogicalTypeHandle::from(LogicalTypeId::Varchar),
        ])
    }
}

/// `CALL p2p_config_reset()` — clear the persisted SQL/runtime layer (defaults).
struct ResetVTab;
impl VTab for ResetVTab {
    type InitData = OnceInit;
    type BindData = Rows3;

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn Error>> {
        add_three_columns(bind, "group", "key", "value");
        let store = ConfigStore::open();
        store.reset().map_err(boxed)?;
        let cfg = store.effective().map_err(boxed)?;
        let mut rows: Vec<[String; 3]> = vec![[
            "result".into(),
            "reset".into(),
            "restored built-in defaults".into(),
        ]];
        rows.extend(p2p_config::status_rows(&cfg).into_iter().map(row3));
        Ok(Rows3 { rows })
    }
    fn init(info: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        once_init(info)
    }
    fn func(
        func: &TableFunctionInfo<Self>,
        output: &mut DataChunkHandle,
    ) -> Result<(), Box<dyn Error>> {
        emit_rows3(func, output)
    }
    fn parameters() -> Option<Vec<LogicalTypeHandle>> {
        Some(vec![])
    }
}

/// Shared gate + plan builder for the provider stake actions. Returns the rows
/// to emit, or a friendly error describing exactly what's missing.
fn stake_action_rows(action: &str, amount: i64) -> Result<Vec<[String; 3]>, Box<dyn Error>> {
    use p2p_config::SettlementRail;
    let cfg = ConfigStore::open().effective().map_err(boxed)?;
    let e = &cfg.economics;

    if amount <= 0 {
        return Err(boxed(format!(
            "{action}: amount must be a positive whole-TON value"
        )));
    }
    if !e.enabled
        || !matches!(
            e.settlement,
            SettlementRail::Onchain | SettlementRail::Channel
        )
    {
        return Err(boxed(format!(
            "{action} requires on-chain settlement. Run \
             `CALL p2p_economics(enabled => true, settlement => 'ton')` first \
             (currently enabled={}, settlement={:?}).",
            e.enabled, e.settlement
        )));
    }
    // Mainnet safety: never act on real funds without explicit confirmation.
    e.guard_mainnet().map_err(boxed)?;

    let settings = e.active_settings();
    let vault = match &settings.contracts.stake_vault {
        Some(v) => v.clone(),
        None => {
            return Err(boxed(format!(
                "{action}: no stake_vault contract registered for {}. Run \
                 `CALL p2p_contracts(stake_vault => 'kQ...')`.",
                e.network.as_str()
            )))
        }
    };
    if settings.wallet.mnemonic_file.is_none() {
        return Err(boxed(format!(
            "{action}: no wallet configured for {}. Run \
             `CALL p2p_wallet(mnemonic_file => '/path/outside/repo')` \
             (never paste the raw mnemonic).",
            e.network.as_str()
        )));
    }

    let status = stake_submit_status(e, &vault, action, amount);
    Ok(vec![
        ["stake".into(), "action".into(), action.into()],
        ["stake".into(), "amount_ton".into(), amount.to_string()],
        ["stake".into(), "network".into(), e.network.as_str().into()],
        ["stake".into(), "stake_vault".into(), vault],
        ["stake".into(), "rpc_endpoint".into(), e.resolved_rpc()],
        ["stake".into(), "status".into(), status],
    ])
}

/// The `status` row value for a stake/unstake action: when built with
/// `--features ton-live`, this self-broadcasts a wallet-v5r1 signed
/// `StakeDeposit`/`StakeRequestUnbond` and reports the toncenter result; without
/// the feature it stays "prepared" (no network).
fn stake_submit_status(
    _e: &p2p_config::EconomicsConfig,
    _vault: &str,
    _action: &str,
    _amount: i64,
) -> String {
    #[cfg(feature = "ton-live")]
    {
        return match broadcast_stake(_e, _vault, _action, _amount) {
            Ok(h) => format!("broadcast — toncenter accepted (tx {h})"),
            Err(err) => format!("broadcast failed: {err}"),
        };
    }
    #[cfg(not(feature = "ton-live"))]
    {
        "prepared — submit on-chain via the configured wallet + RPC (rebuild with --features ton-live to self-broadcast)".into()
    }
}

/// Self-broadcast a stake/unstake message from the configured wallet (ton-live).
#[cfg(feature = "ton-live")]
fn broadcast_stake(
    e: &p2p_config::EconomicsConfig,
    vault: &str,
    action: &str,
    amount: i64,
) -> Result<String, Box<dyn Error>> {
    use p2p_settlement::{TonRpc, WalletAddress};
    let wiring = p2p_settlement::resolve_ton_wiring(e).map_err(boxed)?;
    let path = e
        .active_settings()
        .wallet
        .mnemonic_file
        .clone()
        .ok_or_else(|| boxed("no wallet mnemonic_file configured"))?;
    let mnemonic = std::fs::read_to_string(&path)
        .map_err(|err| boxed(format!("read mnemonic_file {path}: {err}")))?;
    let rpc = p2p_settlement::ToncenterRpc::new(
        &wiring.rpc_endpoint,
        wiring.api_key.clone(),
        &wiring.network,
        mnemonic.trim(),
    )
    .map_err(boxed)?;
    let vault_addr =
        WalletAddress::from_any_str(vault).map_err(|_| boxed("malformed stake_vault address"))?;
    const TON: u128 = 1_000_000_000;
    let amt = (amount.max(0) as u128) * TON;
    let qid = now_secs();
    // Gas/storage headroom (nanoton) attached on top of any bonded value so the
    // vault's COMPUTE phase actually runs on-chain: a 0-value internal message is
    // skipped (cskip_no_gas), and StakeDeposit additionally requires the incoming
    // value to clear `amount + MIN_TONS_FOR_STORAGE` (0.05 TON). 0.1 TON covers
    // the 0.05 storage floor + compute/forward fees with margin.
    const ONCHAIN_GAS: u128 = 100_000_000; // 0.1 TON
    let hash = match action {
        "unstake" => rpc
            .send_internal(
                &vault_addr,
                ONCHAIN_GAS,
                &p2p_settlement::build_stake_unbond(qid, amt),
            )
            .map_err(boxed)?,
        // stake deposit attaches the bonded value PLUS gas/storage headroom so the
        // vault receives `amt + ONCHAIN_GAS` (> amt + 0.05 floor) and bonds `amt`.
        _ => rpc
            .send_internal(
                &vault_addr,
                amt + ONCHAIN_GAS,
                &p2p_settlement::build_stake_deposit(qid, amt),
            )
            .map_err(boxed)?,
    };
    Ok(hash)
}

/// Gate + plan builder for the admin `update_params` action on the on-chain
/// `GlobalParams` contract (BLOCKCHAIN_ECONOMICS §12). Pushes the current,
/// already-validated `[economics]` parameters to the platform-wide contract from
/// the configured admin wallet. The contract is editable in place (address is
/// stable), so this never redeploys.
fn admin_params_rows() -> Result<Vec<[String; 3]>, Box<dyn Error>> {
    use p2p_config::SettlementRail;
    let cfg = ConfigStore::open().effective().map_err(boxed)?;
    let e = &cfg.economics;

    if !e.enabled
        || !matches!(
            e.settlement,
            SettlementRail::Onchain | SettlementRail::Channel
        )
    {
        return Err(boxed(
            "p2p_admin_params requires on-chain settlement. Run \
             `CALL p2p_economics(enabled => true, settlement => 'ton')` first.",
        ));
    }
    // Mainnet safety: never push params to a real-funds network unconfirmed.
    e.guard_mainnet().map_err(boxed)?;
    // The params are derived from the (already cross-field-validated) config.
    e.validate().map_err(boxed)?;

    let settings = e.active_settings();
    let gp = match &settings.contracts.global_params {
        Some(v) => v.clone(),
        None => {
            return Err(boxed(format!(
                "p2p_admin_params: no global_params contract registered for {}. Deploy \
                 GlobalParams, then `CALL p2p_contracts(global_params => 'kQ...')`.",
                e.network.as_str()
            )))
        }
    };
    let admin = match (&settings.wallet.address, &settings.wallet.mnemonic_file) {
        (_, Some(_)) => settings
            .wallet
            .address
            .clone()
            .unwrap_or_else(|| "<from mnemonic>".into()),
        _ => {
            return Err(boxed(format!(
                "p2p_admin_params: no admin wallet configured for {}. Run \
                 `CALL p2p_wallet(mnemonic_file => '/path/outside/repo')`.",
                e.network.as_str()
            )))
        }
    };

    // Surface the §12 params (in their on-chain representation) being pushed.
    let bps = |x: f64| ((x * 10_000.0).round() as i64).to_string();
    let s = &e.slashing;
    let sch = &cfg.scheduler;
    let status = admin_params_submit_status(e, sch, &gp);
    Ok(vec![
        [
            "admin_params".into(),
            "action".into(),
            "update_params".into(),
        ],
        [
            "admin_params".into(),
            "network".into(),
            e.network.as_str().into(),
        ],
        ["admin_params".into(), "global_params".into(), gp],
        ["admin_params".into(), "admin_wallet".into(), admin],
        [
            "admin_params".into(),
            "rpc_endpoint".into(),
            e.resolved_rpc(),
        ],
        [
            "admin_params".into(),
            "platform_fee_bps".into(),
            bps(e.fees.platform_fee_pct),
        ],
        [
            "admin_params".into(),
            "participation_commission_bps".into(),
            bps(e.fees.participation_commission_frac),
        ],
        [
            "admin_params".into(),
            "slash_challenger_bps".into(),
            bps(s.slash_to_challenger),
        ],
        [
            "admin_params".into(),
            "slash_failed_commitment_bps".into(),
            bps(s.slash_failed_commitment_pct),
        ],
        [
            "admin_params".into(),
            "min_stake_ton".into(),
            e.stake.min_stake.to_string(),
        ],
        [
            "admin_params".into(),
            "stake_cap_ton".into(),
            e.stake.stake_cap.to_string(),
        ],
        [
            "admin_params".into(),
            "unbonding_secs".into(),
            e.stake.unbonding_secs.to_string(),
        ],
        [
            "admin_params".into(),
            "challenge_window_secs".into(),
            s.challenge_window_secs.to_string(),
        ],
        // Resilience / fairness gating now promoted on-chain (sourced from
        // [scheduler]) so disputes reference one agreed value.
        [
            "admin_params".into(),
            "attempt_deadline_ms".into(),
            sch.attempt_deadline_ms.to_string(),
        ],
        [
            "admin_params".into(),
            "progress_interval_ms".into(),
            sch.progress_interval_ms.to_string(),
        ],
        [
            "admin_params".into(),
            "progress_stall_mult".into(),
            sch.progress_stall_multiplier.to_string(),
        ],
        [
            "admin_params".into(),
            "fee_recipient".into(),
            e.fee_recipient.clone().unwrap_or_else(|| "<unset>".into()),
        ],
        ["admin_params".into(), "status".into(), status],
    ])
}

/// The `status` row for `p2p_admin_params`: self-broadcasts the admin
/// `update_params` (wallet-v5r1 signed) under `--features ton-live`, else stays
/// "prepared". Mutates `GlobalParams` storage in place (address unchanged).
fn admin_params_submit_status(
    _e: &p2p_config::EconomicsConfig,
    _sched: &p2p_config::SchedulerConfig,
    _gp: &str,
) -> String {
    #[cfg(feature = "ton-live")]
    {
        return match broadcast_admin_params(_e, _sched, _gp) {
            Ok(h) => format!("broadcast — toncenter accepted (tx {h})"),
            Err(err) => format!("broadcast failed: {err}"),
        };
    }
    #[cfg(not(feature = "ton-live"))]
    {
        "prepared — admin update_params via the configured wallet + RPC (rebuild with --features ton-live to self-broadcast)".into()
    }
}

/// Self-broadcast the admin `update_params` to the on-chain `GlobalParams`
/// (ton-live). The §12 params are derived from `[economics]` + `[scheduler]`
/// (single source of truth) and re-validated.
#[cfg(feature = "ton-live")]
fn broadcast_admin_params(
    e: &p2p_config::EconomicsConfig,
    sched: &p2p_config::SchedulerConfig,
    gp: &str,
) -> Result<String, Box<dyn Error>> {
    use p2p_settlement::{GlobalParams, TonRpc, WalletAddress};
    let wiring = p2p_settlement::resolve_ton_wiring(e).map_err(boxed)?;
    let path = e
        .active_settings()
        .wallet
        .mnemonic_file
        .clone()
        .ok_or_else(|| boxed("no admin wallet mnemonic_file configured"))?;
    let mnemonic = std::fs::read_to_string(&path)
        .map_err(|err| boxed(format!("read mnemonic_file {path}: {err}")))?;
    let rpc = p2p_settlement::ToncenterRpc::new(
        &wiring.rpc_endpoint,
        wiring.api_key.clone(),
        &wiring.network,
        mnemonic.trim(),
    )
    .map_err(boxed)?;
    let gp_addr =
        WalletAddress::from_any_str(gp).map_err(|_| boxed("malformed global_params address"))?;
    let fee = e
        .fee_recipient
        .as_deref()
        .ok_or_else(|| boxed("no fee_recipient configured for update_params"))?;
    let fee_addr =
        WalletAddress::from_any_str(fee).map_err(|_| boxed("malformed fee_recipient address"))?;
    let params = GlobalParams::from_config_parts(e, sched);
    params.validate().map_err(boxed)?;
    let body = p2p_settlement::build_update_params(now_secs(), &fee_addr, &params);
    // Attach compute-gas headroom (nanoton): a 0-value internal message to
    // GlobalParams is skipped (cskip_no_gas) and never runs update_params. The
    // contract edits storage in place, so this small value only funds its compute
    // phase (it is not a payout). 0.05 TON is ample for the update.
    rpc.send_internal(&gp_addr, 50_000_000, &body).map_err(boxed)
}

/// `CALL p2p_admin_params()` — push the current `[economics]` params to the
/// on-chain `GlobalParams` contract (admin-gated, edits storage in place).
struct AdminParamsVTab;
impl VTab for AdminParamsVTab {
    type InitData = OnceInit;
    type BindData = Rows3;

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn Error>> {
        add_three_columns(bind, "group", "key", "value");
        Ok(Rows3 {
            rows: admin_params_rows()?,
        })
    }
    fn init(info: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        once_init(info)
    }
    fn func(
        func: &TableFunctionInfo<Self>,
        output: &mut DataChunkHandle,
    ) -> Result<(), Box<dyn Error>> {
        emit_rows3(func, output)
    }
    fn parameters() -> Option<Vec<LogicalTypeHandle>> {
        Some(vec![])
    }
}

/// `CALL p2p_stake(amount => N)` — drive the provider on-chain stake flow.
struct StakeVTab;
impl VTab for StakeVTab {
    type InitData = OnceInit;
    type BindData = Rows3;

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn Error>> {
        add_three_columns(bind, "group", "key", "value");
        let amount = bind
            .get_named_parameter("amount")
            .map(|v| v.to_int64())
            .unwrap_or(0);
        Ok(Rows3 {
            rows: stake_action_rows("stake", amount)?,
        })
    }
    fn init(info: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        once_init(info)
    }
    fn func(
        func: &TableFunctionInfo<Self>,
        output: &mut DataChunkHandle,
    ) -> Result<(), Box<dyn Error>> {
        emit_rows3(func, output)
    }
    fn named_parameters() -> Option<Vec<(String, LogicalTypeHandle)>> {
        Some(vec![(
            "amount".to_string(),
            LogicalTypeHandle::from(LogicalTypeId::Bigint),
        )])
    }
}

/// `CALL p2p_unstake(amount => N)` — request unbonding of staked TON.
struct UnstakeVTab;
impl VTab for UnstakeVTab {
    type InitData = OnceInit;
    type BindData = Rows3;

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn Error>> {
        add_three_columns(bind, "group", "key", "value");
        let amount = bind
            .get_named_parameter("amount")
            .map(|v| v.to_int64())
            .unwrap_or(0);
        Ok(Rows3 {
            rows: stake_action_rows("unstake", amount)?,
        })
    }
    fn init(info: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        once_init(info)
    }
    fn func(
        func: &TableFunctionInfo<Self>,
        output: &mut DataChunkHandle,
    ) -> Result<(), Box<dyn Error>> {
        emit_rows3(func, output)
    }
    fn named_parameters() -> Option<Vec<(String, LogicalTypeHandle)>> {
        Some(vec![(
            "amount".to_string(),
            LogicalTypeHandle::from(LogicalTypeId::Bigint),
        )])
    }
}

// ===========================================================================
// Anti-abuse deny-list surface (ARCHITECTURE "Abuse resistance").
//
// Local deny-lists keyed by node_id / wallet. Each node maintains its OWN list
// and decides independently whom to refuse — no central authority. Persisted to
// blocklist.toml via `p2p_config::BlocklistStore`.
// ===========================================================================

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Infer the kind of an identifier: a `b3:` prefix ⇒ node id, else wallet.
/// An explicit `kind` arg overrides.
fn resolve_kind(explicit: Option<&str>, id: &str) -> BlockKind {
    if let Some(k) = explicit.and_then(BlockKind::parse) {
        return k;
    }
    if id.starts_with("b3:") {
        BlockKind::NodeId
    } else {
        BlockKind::Wallet
    }
}

fn blocklist_rows() -> Result<Vec<[String; 3]>, Box<dyn Error>> {
    let store = BlocklistStore::open();
    let entries = store.list().map_err(boxed)?;
    if entries.is_empty() {
        return Ok(vec![[
            "blocklist".into(),
            "empty".into(),
            "no blocked actors".into(),
        ]]);
    }
    Ok(entries
        .into_iter()
        .map(|e| {
            [
                e.id,
                e.kind.as_str().to_string(),
                format!("{} (source={}, ts={})", e.reason, e.source, e.ts),
            ]
        })
        .collect())
}

/// `CALL p2p_block(id => '...', reason => '...', kind => 'node_id'|'wallet')`.
struct BlockVTab;
impl VTab for BlockVTab {
    type InitData = OnceInit;
    type BindData = Rows3;

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn Error>> {
        add_three_columns(bind, "id", "kind", "detail");
        let id = bind
            .get_named_parameter("id")
            .map(|v| v.to_string())
            .unwrap_or_default();
        let id = id.trim().to_string();
        if id.is_empty() {
            return Err(boxed("p2p_block: an `id` (node_id or wallet) is required"));
        }
        let reason = bind
            .get_named_parameter("reason")
            .map(|v| v.to_string())
            .unwrap_or_else(|| "manual block".to_string());
        let kind_arg = bind.get_named_parameter("kind").map(|v| v.to_string());
        let kind = resolve_kind(kind_arg.as_deref(), &id);
        BlocklistStore::open()
            .block(&id, kind, &reason, "manual", now_secs())
            .map_err(boxed)?;
        let mut rows: Vec<[String; 3]> = vec![["result".into(), "blocked".into(), id.clone()]];
        rows.extend(blocklist_rows()?);
        Ok(Rows3 { rows })
    }
    fn init(info: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        once_init(info)
    }
    fn func(
        func: &TableFunctionInfo<Self>,
        output: &mut DataChunkHandle,
    ) -> Result<(), Box<dyn Error>> {
        emit_rows3(func, output)
    }
    fn named_parameters() -> Option<Vec<(String, LogicalTypeHandle)>> {
        Some(vec![
            (
                "id".to_string(),
                LogicalTypeHandle::from(LogicalTypeId::Varchar),
            ),
            (
                "reason".to_string(),
                LogicalTypeHandle::from(LogicalTypeId::Varchar),
            ),
            (
                "kind".to_string(),
                LogicalTypeHandle::from(LogicalTypeId::Varchar),
            ),
        ])
    }
}

/// `CALL p2p_unblock(id => '...')`.
struct UnblockVTab;
impl VTab for UnblockVTab {
    type InitData = OnceInit;
    type BindData = Rows3;

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn Error>> {
        add_three_columns(bind, "id", "kind", "detail");
        let id = bind
            .get_named_parameter("id")
            .map(|v| v.to_string())
            .unwrap_or_default();
        let id = id.trim().to_string();
        if id.is_empty() {
            return Err(boxed("p2p_unblock: an `id` is required"));
        }
        let removed = BlocklistStore::open().unblock(&id).map_err(boxed)?;
        let mut rows: Vec<[String; 3]> = vec![[
            "result".into(),
            if removed {
                "unblocked".into()
            } else {
                "not_found".into()
            },
            id.clone(),
        ]];
        rows.extend(blocklist_rows()?);
        Ok(Rows3 { rows })
    }
    fn init(info: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        once_init(info)
    }
    fn func(
        func: &TableFunctionInfo<Self>,
        output: &mut DataChunkHandle,
    ) -> Result<(), Box<dyn Error>> {
        emit_rows3(func, output)
    }
    fn named_parameters() -> Option<Vec<(String, LogicalTypeHandle)>> {
        Some(vec![(
            "id".to_string(),
            LogicalTypeHandle::from(LogicalTypeId::Varchar),
        )])
    }
}

/// `SELECT * FROM p2p_blocklist()` — current deny-list entries.
struct BlocklistVTab;
impl VTab for BlocklistVTab {
    type InitData = OnceInit;
    type BindData = Rows3;

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn Error>> {
        add_three_columns(bind, "id", "kind", "detail");
        Ok(Rows3 {
            rows: blocklist_rows()?,
        })
    }
    fn init(info: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        once_init(info)
    }
    fn func(
        func: &TableFunctionInfo<Self>,
        output: &mut DataChunkHandle,
    ) -> Result<(), Box<dyn Error>> {
        emit_rows3(func, output)
    }
    fn parameters() -> Option<Vec<LogicalTypeHandle>> {
        Some(vec![])
    }
}

// ===========================================================================
// Distributed grid surface (architecture §12.0): p2p_query / p2p_share /
// p2p_join, bridged from SQL onto the async `p2p-node` coordinator/worker.
//
// The DuckDB C extension API is **synchronous**; the node is **tokio/async**.
// We stand up ONE managed multi-thread runtime for the whole extension and
// `block_on` node calls from inside the (synchronous) table-function callbacks.
// The node is built lazily on first use from the SQL/runtime config layer
// (`ConfigStore::effective`) and cached; `p2p_join` / `p2p_share` rebuild it so
// new seeds/budget take effect on the live node.
//
// The node needs a `QueryEngine` for its free local-execution path. We CANNOT
// reuse `p2p-node`'s `DuckDbEngine` here: that engine is gated behind the node's
// `duckdb-engine` feature which *bundles* a second DuckDB, conflicting with this
// crate's `loadable-extension` bindings. Instead `HostEngine` runs SQL through
// the **host** DuckDB's own C API (a fresh `:memory:` connection opened via the
// extension API), so no second engine is linked.
// ===========================================================================

/// DuckDB's standard vector size — the max rows emitted per output chunk.
const VECTOR_SIZE: usize = 2048;

/// The shared tokio runtime backing every node call (lazily created, lives for
/// the process). Quinn's QUIC endpoint driver tasks run on its worker threads.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("build tokio runtime for the p2p extension")
    })
}

/// The lazily-built, cached requester/host node.
fn node_cell() -> &'static Mutex<Option<Arc<Node>>> {
    static NODE: OnceLock<Mutex<Option<Arc<Node>>>> = OnceLock::new();
    NODE.get_or_init(|| Mutex::new(None))
}

/// Handle to the running host (worker) accept loop, if `p2p_share` was called.
fn host_cell() -> &'static Mutex<Option<tokio::task::JoinHandle<()>>> {
    static HOST: OnceLock<Mutex<Option<tokio::task::JoinHandle<()>>>> = OnceLock::new();
    HOST.get_or_init(|| Mutex::new(None))
}

/// Build a fresh node from the current effective config (defaults → file → env →
/// SQL/runtime layer). Runs inside the runtime so the QUIC endpoint can bind.
fn build_node() -> std::result::Result<Arc<Node>, String> {
    let cfg = ConfigStore::open().effective().map_err(|e| e.to_string())?;
    let engine: Arc<dyn QueryEngine> = Arc::new(HostEngine::new());

    // Resolve the money rail from `[economics]` (the single construction site).
    // Defaults safely to noop/mock; the on-chain rail is built only in a ton-live
    // build with the live wallet/addresses configured. A resolution error (e.g.
    // unconfirmed mainnet, missing mnemonic) degrades to the free grid rather than
    // failing node construction, so queries still run locally/free.
    let stack = p2p_settlement::resolve_settlement_stack(&cfg.economics).ok();
    let wire_sync = stack
        .as_ref()
        .map(|s| s.params_source.is_some())
        .unwrap_or(false);

    let node = runtime()
        .block_on(async move {
            let mut node = Node::with_config(cfg, engine)?;
            if let Some(stack) = stack {
                node = node.with_settlement_stack(stack);
            }
            // Start the startup + periodic on-chain GlobalParams sync when a read
            // seam was wired (on-chain rail only). 5-minute refresh; free/mock/noop
            // nodes skip it. Spawned inside the runtime so the task is scheduled.
            if wire_sync {
                node.spawn_params_sync(std::time::Duration::from_secs(300));
            }
            Ok::<_, p2p_node::NodeError>(node)
        })
        .map_err(|e| e.to_string())?;
    Ok(Arc::new(node))
}

/// Get the cached node, building it on first use.
fn get_node() -> std::result::Result<Arc<Node>, String> {
    let mut guard = node_cell().lock().unwrap();
    if let Some(n) = guard.as_ref() {
        return Ok(Arc::clone(n));
    }
    let n = build_node()?;
    *guard = Some(Arc::clone(&n));
    Ok(n)
}

/// Rebuild the node from (freshly persisted) config and replace the cache, so a
/// `p2p_join` / `p2p_share` takes effect on the live node for later queries.
fn rebuild_node() -> std::result::Result<Arc<Node>, String> {
    let n = build_node()?;
    *node_cell().lock().unwrap() = Some(Arc::clone(&n));
    Ok(n)
}

/// A `QueryEngine` that executes SQL on the **host** DuckDB via its C API.
///
/// Each call opens a fresh in-process `:memory:` database (through the extension
/// API the host installed at LOAD), applies a budget + minimal lockdown, runs
/// the query and materializes a portable [`ResultSet`]. This is the free
/// local-execution engine behind `p2p_query`'s local path (and the host engine
/// for `p2p_share`). It runs on a blocking thread so the async runtime is not
/// stalled.
struct HostEngine {
    version: String,
}

impl HostEngine {
    fn new() -> Self {
        let version = Connection::open_in_memory()
            .ok()
            .and_then(|c| {
                c.query_row("SELECT library_version FROM pragma_version()", [], |r| {
                    r.get::<_, String>(0)
                })
                .ok()
            })
            .map(|v| format!("duckdb-host-{v}"))
            .unwrap_or_else(|| "duckdb-host".to_string());
        Self { version }
    }

    fn run(sql: &str, lease: ExecLease) -> std::result::Result<ResultSet, EngineError> {
        let conn =
            Connection::open_in_memory().map_err(|e| EngineError::Exec(format!("open: {e}")))?;
        let mb = (lease.memory_bytes / (1024 * 1024)).max(64);
        // Each job gets its OWN private, ephemeral temp dir (0700, unique name)
        // removed when this `TempDir` drops at the end of `run` — never a shared
        // per-process spill dir leaking across jobs/tenants. Set BEFORE the
        // filesystem is disabled (that step validates the local FS), exactly as
        // the node's strict `DuckDbEngine` does.
        let job_tmp = tempfile::Builder::new()
            .prefix("duckton-host-job-")
            .tempdir()
            .map_err(|e| EngineError::Rejected(format!("temp dir: {e}")))?;
        let tmp_path = job_tmp.path().to_string_lossy().replace('\'', "''");
        // Budget + ephemeral temp + the SAME lockdown the node's strict local
        // engine applies (`STRICT_LOCKDOWN_SQL`, shared verbatim so the two
        // engines cannot drift): no auto-install/-load of extensions, no
        // community/unsigned extensions, NO network egress
        // (`enable_external_access=false`), the local filesystem disabled
        // entirely (this free local path opens NO fixtures and must not read
        // e.g. `/etc/passwd`), no unredacted-secret introspection, and finally
        // `lock_configuration=true` so the untrusted query cannot re-open any of
        // it with a later `SET`.
        conn.execute_batch(&format!(
            "SET memory_limit='{mb}MB'; SET threads={threads}; \
             SET temp_directory='{tmp_path}'; {hardening} {lockdown}",
            threads = lease.threads.max(1),
            hardening = p2p_node::EXTENSION_HARDENING_SQL,
            lockdown = p2p_node::STRICT_LOCKDOWN_SQL,
        ))
        .map_err(|e| EngineError::Rejected(format!("engine setup: {e}")))?;

        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| EngineError::Exec(format!("prepare: {e}")))?;
        let mut rows = stmt
            .query([])
            .map_err(|e| EngineError::Exec(format!("query: {e}")))?;
        let columns: Vec<String> = rows.as_ref().map(|s| s.column_names()).unwrap_or_default();
        let ncols = columns.len();
        let mut out: Vec<Vec<PValue>> = Vec::new();
        while let Some(row) = rows
            .next()
            .map_err(|e| EngineError::Exec(format!("fetch: {e}")))?
        {
            let mut r = Vec::with_capacity(ncols);
            for i in 0..ncols {
                let v = row
                    .get_ref(i)
                    .map_err(|e| EngineError::Exec(format!("get col {i}: {e}")))?;
                r.push(value_from_ref(v));
            }
            out.push(r);
        }
        Ok(ResultSet::new(columns, out))
    }
}

#[async_trait::async_trait]
impl QueryEngine for HostEngine {
    async fn execute(
        &self,
        sql: &str,
        lease: ExecLease,
    ) -> std::result::Result<ResultSet, EngineError> {
        let sql = sql.to_string();
        tokio::task::spawn_blocking(move || HostEngine::run(&sql, lease))
            .await
            .map_err(|e| EngineError::Exec(format!("join: {e}")))?
    }

    fn version(&self) -> String {
        self.version.clone()
    }
}

/// Map a host DuckDB value reference into the portable [`PValue`] model.
fn value_from_ref(v: ValueRef<'_>) -> PValue {
    match v {
        ValueRef::Null => PValue::Null,
        ValueRef::Boolean(b) => PValue::Bool(b),
        ValueRef::TinyInt(i) => PValue::Int(i as i64),
        ValueRef::SmallInt(i) => PValue::Int(i as i64),
        ValueRef::Int(i) => PValue::Int(i as i64),
        ValueRef::BigInt(i) => PValue::Int(i),
        ValueRef::HugeInt(i) => i64::try_from(i)
            .map(PValue::Int)
            .unwrap_or_else(|_| PValue::Text(i.to_string())),
        ValueRef::UTinyInt(i) => PValue::Int(i as i64),
        ValueRef::USmallInt(i) => PValue::Int(i as i64),
        ValueRef::UInt(i) => PValue::Int(i as i64),
        ValueRef::UBigInt(i) => i64::try_from(i)
            .map(PValue::Int)
            .unwrap_or_else(|_| PValue::Text(i.to_string())),
        ValueRef::Float(f) => PValue::Float(f as f64),
        ValueRef::Double(f) => PValue::Float(f),
        ValueRef::Text(bytes) => PValue::Text(String::from_utf8_lossy(bytes).to_string()),
        ValueRef::Blob(bytes) => PValue::Blob(bytes.to_vec()),
        other => PValue::Text(format!("{other:?}")),
    }
}

/// Render a portable value as text for the (VARCHAR) result columns. Returns
/// `None` for SQL NULL (the caller sets the row null instead).
fn value_to_string(v: &PValue) -> Option<String> {
    match v {
        PValue::Null => None,
        PValue::Bool(b) => Some(b.to_string()),
        PValue::Int(i) => Some(i.to_string()),
        PValue::Float(f) => Some(f.to_string()),
        PValue::Text(s) => Some(s.clone()),
        PValue::Blob(b) => Some(String::from_utf8_lossy(b).to_string()),
    }
}

/// Parse a human memory size (`'4GB'`, `'512MB'`, `'1048576'`) into bytes.
fn parse_memory(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let (num, mult): (&str, u64) =
        if let Some(n) = s.strip_suffix("gb").or_else(|| s.strip_suffix('g')) {
            (n, 1 << 30)
        } else if let Some(n) = s.strip_suffix("mb").or_else(|| s.strip_suffix('m')) {
            (n, 1 << 20)
        } else if let Some(n) = s.strip_suffix("kb").or_else(|| s.strip_suffix('k')) {
            (n, 1 << 10)
        } else {
            (s.as_str(), 1)
        };
    num.trim()
        .parse::<f64>()
        .ok()
        .map(|x| (x * mult as f64) as u64)
}

/// Collect the per-call `p2p_query` overrides from the supplied named args.
fn query_overrides(bind: &BindInfo) -> QueryOverrides {
    let mut ov = QueryOverrides::default();
    if let Some(v) = bind.get_named_parameter("replicas") {
        ov.replicas = Some(v.to_int64().max(0) as usize);
    }
    if let Some(v) = bind.get_named_parameter("quorum") {
        ov.quorum = Some(v.to_int64().max(0) as usize);
    }
    if let Some(v) = bind.get_named_parameter("min_trust") {
        ov.min_trust = Some(v.to_double());
    }
    if let Some(v) = bind
        .get_named_parameter("min_attest")
        .or_else(|| bind.get_named_parameter("min_attestation"))
    {
        ov.min_attestation = Some(v.to_string().trim().to_uppercase());
    }
    if let Some(v) = bind.get_named_parameter("verify") {
        ov.verify = match v.to_string().trim().to_ascii_lowercase().as_str() {
            "fast" => Some(VerifyModeCfg::Fast),
            "quorum" => Some(VerifyModeCfg::Quorum),
            _ => None,
        };
    }
    if let Some(v) = bind.get_named_parameter("prefer") {
        ov.prefer = match v.to_string().trim().to_ascii_lowercase().as_str() {
            "local" => Some(PreferMode::Local),
            "remote" => Some(PreferMode::Remote),
            "auto" => Some(PreferMode::Auto),
            _ => None,
        };
    }
    if let Some(v) = bind.get_named_parameter("payment") {
        ov.payment = match v.to_string().trim().to_ascii_lowercase().as_str() {
            "free" => Some(PaymentPref::Free),
            "paid" => Some(PaymentPref::Paid),
            "auto" => Some(PaymentPref::Auto),
            _ => None,
        };
    }
    if let Some(v) = bind.get_named_parameter("data_class") {
        ov.data_class = match v.to_string().trim().to_ascii_lowercase().as_str() {
            "public" => Some(DataClassCfg::Public),
            "internal" => Some(DataClassCfg::Internal),
            "sensitive" => Some(DataClassCfg::Sensitive),
            _ => None,
        };
    }
    if let Some(v) = bind.get_named_parameter("require_staked_hosts") {
        ov.require_staked_hosts = Some(matches!(
            v.to_string().trim().to_ascii_lowercase().as_str(),
            "true" | "1" | "yes" | "on"
        ));
    }
    // Request-scoping constraints (§7.5). `network` targets a logical partition;
    // `groups` are the requester's claims; `regions` pins accepted host regions.
    if let Some(v) = bind.get_named_parameter("network") {
        let n = v.to_string().trim().to_string();
        ov.network = if n.is_empty() { None } else { Some(n) };
    }
    if let Some(g) = list_param(bind, "groups") {
        ov.groups = g;
    }
    if let Some(r) = list_param(bind, "regions") {
        ov.regions = r;
    }
    ov
}

/// `FROM p2p_query('SELECT ...', [replicas/quorum/verify/prefer/payment/
/// min_trust/min_attest])` — run a query on the grid (or, on the free local
/// path, in-process), returning the result rows. Columns are emitted as VARCHAR.
struct QueryVTab;

#[repr(C)]
struct QueryBind {
    columns: Vec<String>,
    rows: Vec<Vec<PValue>>,
}

#[repr(C)]
struct QueryInit {
    cursor: AtomicUsize,
}

impl VTab for QueryVTab {
    type InitData = QueryInit;
    type BindData = QueryBind;

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn Error>> {
        let sql = bind.get_parameter(0).to_string();
        let overrides = query_overrides(bind);
        let node = get_node().map_err(boxed)?;
        let outcome = runtime()
            .block_on(async { node.query(&sql, overrides).await })
            .map_err(boxed)?;
        let rs = outcome.result;
        // A result must declare at least one column; synthesize one if the query
        // produced none (it still emits zero rows).
        let columns = if rs.columns.is_empty() {
            vec!["result".to_string()]
        } else {
            rs.columns
        };
        for c in &columns {
            bind.add_result_column(c, LogicalTypeHandle::from(LogicalTypeId::Varchar));
        }
        Ok(QueryBind {
            columns,
            rows: rs.rows,
        })
    }

    fn init(_: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        Ok(QueryInit {
            cursor: AtomicUsize::new(0),
        })
    }

    fn func(
        func: &TableFunctionInfo<Self>,
        output: &mut DataChunkHandle,
    ) -> Result<(), Box<dyn Error>> {
        let init = func.get_init_data();
        let bind = func.get_bind_data();
        let start = init.cursor.load(Ordering::Relaxed);
        let total = bind.rows.len();
        if start >= total {
            output.set_len(0);
            return Ok(());
        }
        let n = (total - start).min(VECTOR_SIZE);
        for col in 0..bind.columns.len() {
            let mut vector = output.flat_vector(col);
            for i in 0..n {
                match bind.rows[start + i].get(col) {
                    Some(PValue::Null) | None => vector.set_null(i),
                    Some(other) => match value_to_string(other) {
                        Some(s) => vector.insert(i, CString::new(s)?),
                        None => vector.set_null(i),
                    },
                }
            }
        }
        output.set_len(n);
        init.cursor.store(start + n, Ordering::Relaxed);
        Ok(())
    }

    fn parameters() -> Option<Vec<LogicalTypeHandle>> {
        Some(vec![LogicalTypeHandle::from(LogicalTypeId::Varchar)])
    }

    fn named_parameters() -> Option<Vec<(String, LogicalTypeHandle)>> {
        Some(query_named_params())
    }
}

/// The per-call named parameters shared by `p2p_query` and `p2p_query_meta`.
fn query_named_params() -> Vec<(String, LogicalTypeHandle)> {
    let v = |id| LogicalTypeHandle::from(id);
    vec![
        ("replicas".to_string(), v(LogicalTypeId::Bigint)),
        ("quorum".to_string(), v(LogicalTypeId::Bigint)),
        ("min_trust".to_string(), v(LogicalTypeId::Double)),
        ("min_attest".to_string(), v(LogicalTypeId::Varchar)),
        ("min_attestation".to_string(), v(LogicalTypeId::Varchar)),
        ("verify".to_string(), v(LogicalTypeId::Varchar)),
        ("prefer".to_string(), v(LogicalTypeId::Varchar)),
        ("payment".to_string(), v(LogicalTypeId::Varchar)),
        ("data_class".to_string(), v(LogicalTypeId::Varchar)),
        ("require_staked_hosts".to_string(), v(LogicalTypeId::Boolean)),
        ("network".to_string(), v(LogicalTypeId::Varchar)),
        (
            "groups".to_string(),
            LogicalTypeHandle::list(&v(LogicalTypeId::Varchar)),
        ),
        (
            "regions".to_string(),
            LogicalTypeHandle::list(&v(LogicalTypeId::Varchar)),
        ),
    ]
}

/// Render a [`p2p_node::QueryOutcome`]'s execution/verification metadata as
/// (group, key, value) rows for `p2p_query_meta` — the introspection companion to
/// `p2p_query`, which returns only the result rows.
fn query_meta_rows(outcome: &p2p_node::QueryOutcome) -> Vec<[String; 3]> {
    let g = "query";
    let winner = outcome
        .winner
        .as_ref()
        .map(|w| w.as_str().to_string())
        .unwrap_or_else(|| "<none>".to_string());
    // Per-job MEASURED performance from the winner's signed receipt (the grid's
    // per-job perf signal): latency + the requester-observed result bytes and the
    // estimated scanned input bytes. Falls back to any Correct receipt, else the
    // unknown (`0`) sentinel.
    let winner_receipt = outcome
        .winner
        .as_ref()
        .and_then(|w| {
            outcome
                .receipts
                .iter()
                .find(|r| &r.worker_id == w && r.verdict == p2p_proto::Verdict::Correct)
        })
        .or_else(|| {
            outcome
                .receipts
                .iter()
                .find(|r| r.verdict == p2p_proto::Verdict::Correct)
        });
    let (latency_ms, observed_input_bytes, observed_result_bytes) = winner_receipt
        .map(|r| {
            (
                r.latency_ms,
                r.observed_input_bytes,
                r.observed_result_bytes,
            )
        })
        .unwrap_or((0, 0, 0));
    vec![
        [g.into(), "job_id".into(), outcome.job_id.0.clone()],
        [
            g.into(),
            "executed_locally".into(),
            outcome.executed_locally.to_string(),
        ],
        [g.into(), "verified".into(), outcome.verified.to_string()],
        [g.into(), "agreement".into(), outcome.agreement.to_string()],
        [g.into(), "quorum".into(), outcome.quorum.to_string()],
        [g.into(), "winner".into(), winner],
        [
            g.into(),
            "participants".into(),
            outcome.participants.len().to_string(),
        ],
        [g.into(), "receipts".into(), outcome.receipts.len().to_string()],
        [
            g.into(),
            "agreed_hash".into(),
            outcome
                .agreed_hash
                .clone()
                .unwrap_or_else(|| "<none>".to_string()),
        ],
        [
            g.into(),
            "result_rows".into(),
            outcome.result.row_count().to_string(),
        ],
        // Per-job perf measurement (winner receipt). `0` = unknown.
        [g.into(), "latency_ms".into(), latency_ms.to_string()],
        [
            g.into(),
            "observed_input_bytes".into(),
            observed_input_bytes.to_string(),
        ],
        [
            g.into(),
            "observed_result_bytes".into(),
            observed_result_bytes.to_string(),
        ],
    ]
}

/// `FROM p2p_query_meta('SELECT ...', [same named params as p2p_query])` — run a
/// query and return its execution/verification METADATA (executed_locally,
/// verified, agreement/quorum, winner, participant/receipt counts, agreed hash,
/// result row count) as (group, key, value) rows. Back-compatible companion to
/// `p2p_query`, whose row shape is unchanged (it still returns only result rows).
struct QueryMetaVTab;
impl VTab for QueryMetaVTab {
    type InitData = OnceInit;
    type BindData = Rows3;

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn Error>> {
        add_three_columns(bind, "group", "key", "value");
        let sql = bind.get_parameter(0).to_string();
        let overrides = query_overrides(bind);
        let node = get_node().map_err(boxed)?;
        let outcome = runtime()
            .block_on(async { node.query(&sql, overrides).await })
            .map_err(boxed)?;
        Ok(Rows3 {
            rows: query_meta_rows(&outcome),
        })
    }

    fn init(info: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        once_init(info)
    }

    fn func(
        func: &TableFunctionInfo<Self>,
        output: &mut DataChunkHandle,
    ) -> Result<(), Box<dyn Error>> {
        emit_rows3(func, output)
    }

    fn parameters() -> Option<Vec<LogicalTypeHandle>> {
        Some(vec![LogicalTypeHandle::from(LogicalTypeId::Varchar)])
    }

    fn named_parameters() -> Option<Vec<(String, LogicalTypeHandle)>> {
        Some(query_named_params())
    }
}

/// `FROM p2p_node_metadata()` — emit this node's non-GDPR `SystemProfile`
/// (machine class: CPU/RAM/disk/OS shape, donated compute budget, resource
/// ceilings) as (group, key, value) rows. Prefers the host-collected, signed
/// profile persisted at host start; falls back to collecting one on demand for a
/// requester-only node.
///
/// This is a SELF-REPORTED analytics / routing HINT only — it never feeds
/// trust/selection scoring (which stays receipt-driven).
struct NodeMetadataVTab;
impl VTab for NodeMetadataVTab {
    type InitData = OnceInit;
    type BindData = Rows3;

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn Error>> {
        add_three_columns(bind, "group", "key", "value");
        let node = get_node().map_err(boxed)?;
        let transport = node.coordinator().transport();
        let signer = p2p_node::IdentitySigner(transport.identity());
        // Surface the persisted, host-collected profile when present; otherwise
        // collect a fresh snapshot on demand (requester-only node).
        let profile = p2p_node::SystemStore::open()
            .load_verified(node.node_id())
            .unwrap_or_else(|| {
                let engine_version = HostEngine::new().version();
                p2p_node::collect_system_profile(
                    &signer,
                    &node.config().budget,
                    &engine_version,
                    env!("CARGO_PKG_VERSION"),
                )
            });
        Ok(Rows3 {
            rows: profile.metadata_rows(),
        })
    }

    fn init(info: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        once_init(info)
    }

    fn func(
        func: &TableFunctionInfo<Self>,
        output: &mut DataChunkHandle,
    ) -> Result<(), Box<dyn Error>> {
        emit_rows3(func, output)
    }

    fn parameters() -> Option<Vec<LogicalTypeHandle>> {
        Some(vec![])
    }
}

/// Read a `LIST(VARCHAR)` named parameter into a `Vec<String>` (drops NULLs).
fn list_param(bind: &BindInfo, name: &str) -> Option<Vec<String>> {
    let val = bind.get_named_parameter(name)?;
    let items = val.to_list()?;
    Some(
        items
            .iter()
            .filter(|v| !v.is_null())
            .map(|v| v.to_string())
            .collect(),
    )
}

/// `CALL p2p_share(memory => '4GB', threads => 2, max_jobs => 3,
/// data_classes => ['public'])` — become a host: persist the donated budget,
/// (re)build the live node and spawn its worker accept loop so it serves the
/// grid.
struct ShareVTab;
impl VTab for ShareVTab {
    type InitData = OnceInit;
    type BindData = Rows3;

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn Error>> {
        add_three_columns(bind, "group", "key", "value");
        let store = ConfigStore::open();

        // Persist the donated budget into the runtime layer.
        let mut pairs: Vec<(String, toml::Value)> = Vec::new();
        if let Some(v) = bind.get_named_parameter("memory") {
            let raw = v.to_string();
            let bytes = parse_memory(&raw)
                .ok_or_else(|| boxed(format!("p2p_share: could not parse memory '{raw}'")))?;
            pairs.push((
                "budget.memory_bytes".into(),
                toml::Value::Integer(bytes as i64),
            ));
        }
        if let Some(v) = bind.get_named_parameter("threads") {
            pairs.push((
                "budget.threads".into(),
                toml::Value::Integer(v.to_int64().max(1)),
            ));
        }
        if let Some(v) = bind.get_named_parameter("max_jobs") {
            pairs.push((
                "budget.max_jobs".into(),
                toml::Value::Integer(v.to_int64().max(1)),
            ));
        }
        if let Some(classes) = list_param(bind, "data_classes") {
            // Validate against the known data classes before persisting.
            for c in &classes {
                if !matches!(
                    c.to_ascii_lowercase().as_str(),
                    "public" | "internal" | "sensitive"
                ) {
                    return Err(boxed(format!(
                        "p2p_share: unknown data class '{c}' (public|internal|sensitive)"
                    )));
                }
            }
            let arr = classes
                .iter()
                .map(|c| toml::Value::String(c.to_ascii_lowercase()))
                .collect();
            pairs.push(("budget.data_classes".into(), toml::Value::Array(arr)));
        }
        // Request-scoping labels (§7.5): the host's serving state + partition /
        // groups / region. All optional — omitting them leaves today's posture.
        if let Some(v) = bind.get_named_parameter("enabled") {
            let on = matches!(
                v.to_string().trim().to_ascii_lowercase().as_str(),
                "true" | "1" | "yes" | "on"
            );
            pairs.push(("worker.enabled".into(), toml::Value::Boolean(on)));
        }
        if let Some(nets) = list_param(bind, "networks") {
            let arr = nets.iter().cloned().map(toml::Value::String).collect();
            pairs.push(("membership.networks".into(), toml::Value::Array(arr)));
        }
        if let Some(groups) = list_param(bind, "groups") {
            let arr = groups.iter().cloned().map(toml::Value::String).collect();
            pairs.push(("membership.groups".into(), toml::Value::Array(arr)));
        }
        if let Some(v) = bind.get_named_parameter("region") {
            let r = v.to_string().trim().to_string();
            if !r.is_empty() {
                pairs.push(("membership.region".into(), toml::Value::String(r)));
            }
        }
        if !pairs.is_empty() {
            store.set(&pairs).map_err(boxed)?;
        }

        // (Re)build the node from the new config and start hosting.
        let node = rebuild_node().map_err(boxed)?;
        let handle = {
            let _g = runtime().enter();
            node.spawn_host()
        };
        let mut host = host_cell().lock().unwrap();
        if let Some(old) = host.take() {
            old.abort();
        }
        *host = Some(handle);

        let cfg = node.config();
        let addr = node
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| "<unbound>".into());
        let rows = vec![
            ["share".into(), "status".into(), "hosting".into()],
            [
                "share".into(),
                "node_id".into(),
                node.node_id().as_str().to_string(),
            ],
            ["share".into(), "listen_addr".into(), addr],
            [
                "share".into(),
                "memory_bytes".into(),
                cfg.budget.memory_bytes.to_string(),
            ],
            [
                "share".into(),
                "threads".into(),
                cfg.budget.threads.to_string(),
            ],
            [
                "share".into(),
                "max_jobs".into(),
                cfg.budget.max_jobs.to_string(),
            ],
            [
                "share".into(),
                "data_classes".into(),
                cfg.budget
                    .data_classes
                    .iter()
                    .map(|c| format!("{c:?}").to_lowercase())
                    .collect::<Vec<_>>()
                    .join(","),
            ],
            [
                "share".into(),
                "enabled".into(),
                cfg.worker.enabled.to_string(),
            ],
            [
                "share".into(),
                "networks".into(),
                cfg.membership.networks.join(","),
            ],
            [
                "share".into(),
                "groups".into(),
                cfg.membership.groups.join(","),
            ],
            [
                "share".into(),
                "region".into(),
                cfg.membership.region.clone().unwrap_or_default(),
            ],
        ];
        Ok(Rows3 { rows })
    }

    fn init(info: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        once_init(info)
    }

    fn func(
        func: &TableFunctionInfo<Self>,
        output: &mut DataChunkHandle,
    ) -> Result<(), Box<dyn Error>> {
        emit_rows3(func, output)
    }

    fn named_parameters() -> Option<Vec<(String, LogicalTypeHandle)>> {
        let list_varchar =
            || LogicalTypeHandle::list(&LogicalTypeHandle::from(LogicalTypeId::Varchar));
        Some(vec![
            (
                "memory".to_string(),
                LogicalTypeHandle::from(LogicalTypeId::Varchar),
            ),
            (
                "threads".to_string(),
                LogicalTypeHandle::from(LogicalTypeId::Bigint),
            ),
            (
                "max_jobs".to_string(),
                LogicalTypeHandle::from(LogicalTypeId::Bigint),
            ),
            ("data_classes".to_string(), list_varchar()),
            (
                "enabled".to_string(),
                LogicalTypeHandle::from(LogicalTypeId::Boolean),
            ),
            ("networks".to_string(), list_varchar()),
            ("groups".to_string(), list_varchar()),
            (
                "region".to_string(),
                LogicalTypeHandle::from(LogicalTypeId::Varchar),
            ),
        ])
    }
}

/// Flip this host's serving state and (re)build so it takes effect. Shared by
/// `p2p_pause` (`enabled=false`) and `p2p_resume` (`enabled=true`). A standby host
/// declines NEW offers at admission; an executing job is never interrupted by the
/// flag (the graceful-drain guarantee lives in the worker's `make_bid`).
fn set_serving(enabled: bool) -> std::result::Result<Vec<[String; 3]>, String> {
    ConfigStore::open()
        .set(&[("worker.enabled".into(), toml::Value::Boolean(enabled))])
        .map_err(|e| e.to_string())?;
    let node = rebuild_node()?;
    let handle = {
        let _g = runtime().enter();
        node.spawn_host()
    };
    let mut host = host_cell().lock().unwrap();
    if let Some(old) = host.take() {
        old.abort();
    }
    *host = Some(handle);
    let state = if enabled { "serving" } else { "standby" };
    Ok(vec![
        ["membership".into(), "status".into(), state.into()],
        ["membership".into(), "enabled".into(), enabled.to_string()],
        [
            "membership".into(),
            "node_id".into(),
            node.node_id().as_str().to_string(),
        ],
    ])
}

/// `CALL p2p_pause()` — graceful standby: stop accepting NEW offers (in-flight
/// jobs finish). Sugar over `p2p_share(enabled => false)`.
struct PauseVTab;
impl VTab for PauseVTab {
    type InitData = OnceInit;
    type BindData = Rows3;

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn Error>> {
        add_three_columns(bind, "group", "key", "value");
        let rows = set_serving(false).map_err(boxed)?;
        Ok(Rows3 { rows })
    }
    fn init(info: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        once_init(info)
    }
    fn func(
        func: &TableFunctionInfo<Self>,
        output: &mut DataChunkHandle,
    ) -> Result<(), Box<dyn Error>> {
        emit_rows3(func, output)
    }
    fn parameters() -> Option<Vec<LogicalTypeHandle>> {
        Some(vec![])
    }
}

/// `CALL p2p_resume()` — resume serving after a `p2p_pause`. Sugar over
/// `p2p_share(enabled => true)`.
struct ResumeVTab;
impl VTab for ResumeVTab {
    type InitData = OnceInit;
    type BindData = Rows3;

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn Error>> {
        add_three_columns(bind, "group", "key", "value");
        let rows = set_serving(true).map_err(boxed)?;
        Ok(Rows3 { rows })
    }
    fn init(info: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        once_init(info)
    }
    fn func(
        func: &TableFunctionInfo<Self>,
        output: &mut DataChunkHandle,
    ) -> Result<(), Box<dyn Error>> {
        emit_rows3(func, output)
    }
    fn parameters() -> Option<Vec<LogicalTypeHandle>> {
        Some(vec![])
    }
}

/// `CALL p2p_join(bootstrap => ['quic://seed:9494', ...])` — join a swarm:
/// persist the bootstrap seeds and (re)build the live node so discovery fans out
/// to them for subsequent `p2p_query` calls.
struct JoinVTab;
impl VTab for JoinVTab {
    type InitData = OnceInit;
    type BindData = Rows3;

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn Error>> {
        add_three_columns(bind, "group", "key", "value");
        let seeds = list_param(bind, "bootstrap").unwrap_or_default();
        if seeds.is_empty() {
            return Err(boxed(
                "p2p_join: provide one or more seeds, e.g. \
                 p2p_join(bootstrap => ['quic://seed1:9494'])",
            ));
        }
        let arr = seeds.iter().cloned().map(toml::Value::String).collect();
        ConfigStore::open()
            .set(&[("discovery.bootstrap".into(), toml::Value::Array(arr))])
            .map_err(boxed)?;

        // Rebuild so the new seeds drive discovery on the live node.
        let node = rebuild_node().map_err(boxed)?;
        let cfg = node.config();
        let mut rows = vec![
            ["join".into(), "status".into(), "joined".into()],
            [
                "join".into(),
                "node_id".into(),
                node.node_id().as_str().to_string(),
            ],
            [
                "join".into(),
                "candidate_sample_size".into(),
                cfg.discovery.candidate_sample_size.to_string(),
            ],
        ];
        for s in &cfg.discovery.bootstrap {
            rows.push(["join".into(), "bootstrap".into(), s.clone()]);
        }
        Ok(Rows3 { rows })
    }

    fn init(info: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        once_init(info)
    }

    fn func(
        func: &TableFunctionInfo<Self>,
        output: &mut DataChunkHandle,
    ) -> Result<(), Box<dyn Error>> {
        emit_rows3(func, output)
    }

    fn named_parameters() -> Option<Vec<(String, LogicalTypeHandle)>> {
        let list_varchar =
            LogicalTypeHandle::list(&LogicalTypeHandle::from(LogicalTypeId::Varchar));
        Some(vec![("bootstrap".to_string(), list_varchar)])
    }
}

#[duckdb_entrypoint_c_api(ext_name = "duckton", min_duckdb_version = "v1.0.0")]
pub fn duckton_init(con: Connection) -> Result<(), Box<dyn Error>> {
    // Read-only metadata + inspection.
    con.register_table_function::<InfoVTab>("p2p_info")?;
    con.register_table_function::<PeersVTab>("p2p_peers")?;
    con.register_table_function::<ConfigInspectVTab>("p2p_config")?;
    con.register_table_function::<ConfigInspectVTab>("p2p_settings")?;
    con.register_table_function::<StatusVTab>("p2p_status")?;

    // Grouped, friendly setters (CALL ...).
    con.register_table_function::<EconomicsVTab>("p2p_economics")?;
    con.register_table_function::<PricingVTab>("p2p_pricing")?;
    con.register_table_function::<BiddingVTab>("p2p_bidding")?;
    con.register_table_function::<SelectionVTab>("p2p_selection")?;
    con.register_table_function::<FeesVTab>("p2p_fees")?;
    con.register_table_function::<TrustVTab>("p2p_trust")?;
    con.register_table_function::<PlannerVTab>("p2p_planner")?;
    con.register_table_function::<ContractsVTab>("p2p_contracts")?;
    con.register_table_function::<WalletVTab>("p2p_wallet")?;

    // Generic escape hatch + reset + provider stake actions.
    con.register_table_function::<SetVTab>("p2p_set")?;
    con.register_table_function::<ResetVTab>("p2p_config_reset")?;
    con.register_table_function::<StakeVTab>("p2p_stake")?;
    con.register_table_function::<UnstakeVTab>("p2p_unstake")?;
    con.register_table_function::<AdminParamsVTab>("p2p_admin_params")?;

    // Anti-abuse deny-list surface.
    con.register_table_function::<BlockVTab>("p2p_block")?;
    con.register_table_function::<UnblockVTab>("p2p_unblock")?;
    con.register_table_function::<BlocklistVTab>("p2p_blocklist")?;

    // Distributed grid surface: run queries on the grid / locally, become a
    // host, join a swarm. These drive the live async `p2p-node`.
    con.register_table_function::<QueryVTab>("p2p_query")?;
    con.register_table_function::<QueryMetaVTab>("p2p_query_meta")?;
    con.register_table_function::<NodeMetadataVTab>("p2p_node_metadata")?;
    con.register_table_function::<ShareVTab>("p2p_share")?;
    con.register_table_function::<PauseVTab>("p2p_pause")?;
    con.register_table_function::<ResumeVTab>("p2p_resume")?;
    con.register_table_function::<JoinVTab>("p2p_join")?;
    Ok(())
}
