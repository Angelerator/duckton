//! `duckdb_p2p` — the loadable DuckDB C-API extension surface (architecture §12).
//!
//! Phase 0 walking-skeleton surface. It is built as a loadable extension against
//! DuckDB's **stable C extension API** (so it loads via `LOAD 'duckdb_p2p'`
//! without linking the whole engine). It exposes table functions that prove the
//! extension loads and is wired to the workspace crates:
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
use std::sync::atomic::{AtomicBool, Ordering};

use duckdb::core::{DataChunkHandle, Inserter, LogicalTypeHandle, LogicalTypeId};
use duckdb::vtab::{BindInfo, InitInfo, TableFunctionInfo, VTab};
use duckdb::{duckdb_entrypoint_c_api, Connection, Result};

use p2p_config::{BlockKind, BlocklistStore, ConfigStore, SettingRow};

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
            ("protocol_name".to_string(), p2p_proto::PROTOCOL_NAME.to_string()),
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

    fn func(func: &TableFunctionInfo<Self>, output: &mut DataChunkHandle) -> Result<(), Box<dyn Error>> {
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
                    ("discovery_mode".to_string(), format!("{:?}", cfg.discovery.mode)),
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

    fn func(func: &TableFunctionInfo<Self>, output: &mut DataChunkHandle) -> Result<(), Box<dyn Error>> {
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
fn emit_two_columns(output: &mut DataChunkHandle, rows: &[(String, String)]) -> Result<(), Box<dyn Error>> {
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

fn emit_three_columns(output: &mut DataChunkHandle, rows: &[[String; 3]]) -> Result<(), Box<dyn Error>> {
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
fn emit_rows3<T>(func: &TableFunctionInfo<T>, output: &mut DataChunkHandle) -> Result<(), Box<dyn Error>>
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

group_setter!(EconomicsVTab, "economics", [
    ("enabled", Boolean),
    ("settlement", Varchar),
    ("network", Varchar),
    ("confirm", Boolean),
    ("fee_recipient", Varchar),
    ("default_payment", Varchar),
]);
group_setter!(PricingVTab, "pricing", [
    ("unit_price", Bigint),
    ("max_bid", Bigint),
]);
group_setter!(BiddingVTab, "bidding", [
    ("w_quality", Double),
    ("w_stake", Double),
    ("w_price", Double),
]);
group_setter!(SelectionVTab, "selection", [
    ("replicas", Bigint),
    ("quorum", Bigint),
    ("checksum_min", Bigint),
    ("n_public", Bigint),
    ("n_default", Bigint),
    ("n_max", Bigint),
]);
group_setter!(FeesVTab, "fees", [
    ("platform_fee_pct", Double),
    ("participation_commission_frac", Double),
    ("verification_surcharge_pct", Double),
    ("bonus_aggressiveness", Double),
]);
group_setter!(TrustVTab, "trust", [
    ("min_trust", Double),
    ("min_attest", Varchar),
    ("min_attestation", Varchar),
]);
group_setter!(ContractsVTab, "contracts", [
    ("stake_vault", Varchar),
    ("job_escrow", Varchar),
    ("record_anchor", Varchar),
    ("global_params", Varchar),
]);
group_setter!(WalletVTab, "wallet", [
    ("rpc", Varchar),
    ("address", Varchar),
    ("mnemonic_file", Varchar),
    ("api_key_file", Varchar),
    ("mnemonic", Varchar),
    ("api_key", Varchar),
]);

/// `SELECT * FROM p2p_config()` / `p2p_settings()` — effective settings, grouped
/// and human-readable, with secrets redacted.
struct ConfigInspectVTab;
impl VTab for ConfigInspectVTab {
    type InitData = OnceInit;
    type BindData = Rows3;

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn Error>> {
        add_three_columns(bind, "group", "key", "value");
        let rows = ConfigStore::open().settings().map_err(boxed)?.into_iter().map(row3).collect();
        Ok(Rows3 { rows })
    }
    fn init(info: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        once_init(info)
    }
    fn func(func: &TableFunctionInfo<Self>, output: &mut DataChunkHandle) -> Result<(), Box<dyn Error>> {
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
        let rows = ConfigStore::open().status().map_err(boxed)?.into_iter().map(row3).collect();
        Ok(Rows3 { rows })
    }
    fn init(info: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        once_init(info)
    }
    fn func(func: &TableFunctionInfo<Self>, output: &mut DataChunkHandle) -> Result<(), Box<dyn Error>> {
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
        let rows = p2p_config::status_rows(&cfg).into_iter().map(row3).collect();
        Ok(Rows3 { rows })
    }
    fn init(info: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        once_init(info)
    }
    fn func(func: &TableFunctionInfo<Self>, output: &mut DataChunkHandle) -> Result<(), Box<dyn Error>> {
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
        let mut rows: Vec<[String; 3]> =
            vec![["result".into(), "reset".into(), "restored built-in defaults".into()]];
        rows.extend(p2p_config::status_rows(&cfg).into_iter().map(row3));
        Ok(Rows3 { rows })
    }
    fn init(info: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        once_init(info)
    }
    fn func(func: &TableFunctionInfo<Self>, output: &mut DataChunkHandle) -> Result<(), Box<dyn Error>> {
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
        return Err(boxed(format!("{action}: amount must be a positive whole-TON value")));
    }
    if !e.enabled || !matches!(e.settlement, SettlementRail::Onchain | SettlementRail::Channel) {
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

    Ok(vec![
        ["stake".into(), "action".into(), action.into()],
        ["stake".into(), "amount_ton".into(), amount.to_string()],
        ["stake".into(), "network".into(), e.network.as_str().into()],
        ["stake".into(), "stake_vault".into(), vault],
        ["stake".into(), "rpc_endpoint".into(), e.resolved_rpc()],
        [
            "stake".into(),
            "status".into(),
            // Honest boundary: the on-chain submission goes through the live TON
            // client, which is wired separately (see report follow-up).
            "prepared — submit on-chain via the configured wallet + RPC (live TON client)".into(),
        ],
    ])
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

    if !e.enabled || !matches!(e.settlement, SettlementRail::Onchain | SettlementRail::Channel) {
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
        (_, Some(_)) => settings.wallet.address.clone().unwrap_or_else(|| "<from mnemonic>".into()),
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
    Ok(vec![
        ["admin_params".into(), "action".into(), "update_params".into()],
        ["admin_params".into(), "network".into(), e.network.as_str().into()],
        ["admin_params".into(), "global_params".into(), gp],
        ["admin_params".into(), "admin_wallet".into(), admin],
        ["admin_params".into(), "rpc_endpoint".into(), e.resolved_rpc()],
        ["admin_params".into(), "platform_fee_bps".into(), bps(e.fees.platform_fee_pct)],
        [
            "admin_params".into(),
            "participation_commission_bps".into(),
            bps(e.fees.participation_commission_frac),
        ],
        ["admin_params".into(), "slash_challenger_bps".into(), bps(s.slash_to_challenger)],
        ["admin_params".into(), "min_stake_ton".into(), e.stake.min_stake.to_string()],
        ["admin_params".into(), "stake_cap_ton".into(), e.stake.stake_cap.to_string()],
        ["admin_params".into(), "unbonding_secs".into(), e.stake.unbonding_secs.to_string()],
        [
            "admin_params".into(),
            "challenge_window_secs".into(),
            s.challenge_window_secs.to_string(),
        ],
        [
            "admin_params".into(),
            "fee_recipient".into(),
            e.fee_recipient.clone().unwrap_or_else(|| "<unset>".into()),
        ],
        [
            "admin_params".into(),
            "status".into(),
            // Honest boundary: the on-chain submission goes through the live TON
            // client (admin wallet signs `update_params`); mutates storage in
            // place so the GlobalParams address is unchanged.
            "prepared — admin update_params via the configured wallet + RPC (live TON client)".into(),
        ],
    ])
}

/// `CALL p2p_admin_params()` — push the current `[economics]` params to the
/// on-chain `GlobalParams` contract (admin-gated, edits storage in place).
struct AdminParamsVTab;
impl VTab for AdminParamsVTab {
    type InitData = OnceInit;
    type BindData = Rows3;

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn Error>> {
        add_three_columns(bind, "group", "key", "value");
        Ok(Rows3 { rows: admin_params_rows()? })
    }
    fn init(info: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        once_init(info)
    }
    fn func(func: &TableFunctionInfo<Self>, output: &mut DataChunkHandle) -> Result<(), Box<dyn Error>> {
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
        let amount = bind.get_named_parameter("amount").map(|v| v.to_int64()).unwrap_or(0);
        Ok(Rows3 {
            rows: stake_action_rows("stake", amount)?,
        })
    }
    fn init(info: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        once_init(info)
    }
    fn func(func: &TableFunctionInfo<Self>, output: &mut DataChunkHandle) -> Result<(), Box<dyn Error>> {
        emit_rows3(func, output)
    }
    fn named_parameters() -> Option<Vec<(String, LogicalTypeHandle)>> {
        Some(vec![("amount".to_string(), LogicalTypeHandle::from(LogicalTypeId::Bigint))])
    }
}

/// `CALL p2p_unstake(amount => N)` — request unbonding of staked TON.
struct UnstakeVTab;
impl VTab for UnstakeVTab {
    type InitData = OnceInit;
    type BindData = Rows3;

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn Error>> {
        add_three_columns(bind, "group", "key", "value");
        let amount = bind.get_named_parameter("amount").map(|v| v.to_int64()).unwrap_or(0);
        Ok(Rows3 {
            rows: stake_action_rows("unstake", amount)?,
        })
    }
    fn init(info: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        once_init(info)
    }
    fn func(func: &TableFunctionInfo<Self>, output: &mut DataChunkHandle) -> Result<(), Box<dyn Error>> {
        emit_rows3(func, output)
    }
    fn named_parameters() -> Option<Vec<(String, LogicalTypeHandle)>> {
        Some(vec![("amount".to_string(), LogicalTypeHandle::from(LogicalTypeId::Bigint))])
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
        return Ok(vec![["blocklist".into(), "empty".into(), "no blocked actors".into()]]);
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
        let mut rows: Vec<[String; 3]> =
            vec![["result".into(), "blocked".into(), id.clone()]];
        rows.extend(blocklist_rows()?);
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
            ("id".to_string(), LogicalTypeHandle::from(LogicalTypeId::Varchar)),
            ("reason".to_string(), LogicalTypeHandle::from(LogicalTypeId::Varchar)),
            ("kind".to_string(), LogicalTypeHandle::from(LogicalTypeId::Varchar)),
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
            if removed { "unblocked".into() } else { "not_found".into() },
            id.clone(),
        ]];
        rows.extend(blocklist_rows()?);
        Ok(Rows3 { rows })
    }
    fn init(info: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        once_init(info)
    }
    fn func(func: &TableFunctionInfo<Self>, output: &mut DataChunkHandle) -> Result<(), Box<dyn Error>> {
        emit_rows3(func, output)
    }
    fn named_parameters() -> Option<Vec<(String, LogicalTypeHandle)>> {
        Some(vec![("id".to_string(), LogicalTypeHandle::from(LogicalTypeId::Varchar))])
    }
}

/// `SELECT * FROM p2p_blocklist()` — current deny-list entries.
struct BlocklistVTab;
impl VTab for BlocklistVTab {
    type InitData = OnceInit;
    type BindData = Rows3;

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn Error>> {
        add_three_columns(bind, "id", "kind", "detail");
        Ok(Rows3 { rows: blocklist_rows()? })
    }
    fn init(info: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        once_init(info)
    }
    fn func(func: &TableFunctionInfo<Self>, output: &mut DataChunkHandle) -> Result<(), Box<dyn Error>> {
        emit_rows3(func, output)
    }
    fn parameters() -> Option<Vec<LogicalTypeHandle>> {
        Some(vec![])
    }
}

#[duckdb_entrypoint_c_api(ext_name = "duckdb_p2p", min_duckdb_version = "v1.0.0")]
pub fn duckdb_p2p_init(con: Connection) -> Result<(), Box<dyn Error>> {
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
    Ok(())
}
