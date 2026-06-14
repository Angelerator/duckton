//! SQL-driven configuration surface — the persisted **runtime** config layer
//! (architecture §12 SQL admin API, §17 config layering).
//!
//! A non-technical ("business") user can set up and manage everything —
//! blockchain on/off, network mode, wallet refs, pricing, bidding, stake,
//! fees, trust/selection, contract addresses — entirely via SQL `CALL`s, with no
//! TOML/env editing. [`ConfigStore`] is the engine behind those calls.
//!
//! ## Layering
//! The SQL settings are a **persisted runtime layer** that sits *above* env in
//! the existing precedence:
//!   defaults → config file (`P2P_CONFIG`) → `P2P_*` env → **SQL/runtime** → per-call.
//! They are written to a sparse runtime overrides file (default
//! `<config-dir>/runtime.toml`, override with `P2P_RUNTIME_CONFIG`) so they
//! survive restart. Each change is validated through the typed [`GridConfig`],
//! so a bad value is rejected with a friendly, actionable message — never a
//! panic. The base hand-edited file is never rewritten.
//!
//! ## Secrets
//! Secrets (mnemonic, API key) are NEVER written to the config file and NEVER
//! echoed. The wallet setters store only **references** (a public address + a
//! path to a `0600` secret file kept outside the repo). A raw inline secret is
//! written to that protected file and only its path is persisted. Inspection
//! redacts any secret-named field.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use toml::value::Table;
use toml::Value;

use crate::economics::TonNetwork;
use crate::{ConfigError, GridConfig};

/// Errors from the SQL/runtime config surface. All messages are user-facing and
/// actionable (no panics / stack traces leak to the SQL caller).
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("config error: {0}")]
    Config(#[from] ConfigError),
    #[error("invalid setting: {0}")]
    BadParam(String),
    #[error("blocked: {0}")]
    Blocked(String),
    #[error("config file I/O error at {0}: {1}")]
    Io(String, String),
}

/// One human-readable row of effective settings / status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingRow {
    /// The group / section (e.g. `economics`, `trust`, `status`).
    pub group: String,
    /// The dotted key within the group (e.g. `fees.platform_fee_pct`).
    pub key: String,
    /// The display value (secrets shown as `<redacted>`).
    pub value: String,
}

impl SettingRow {
    fn new(group: impl Into<String>, key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            group: group.into(),
            key: key.into(),
            value: value.into(),
        }
    }
}

/// The persisted SQL/runtime configuration store.
pub struct ConfigStore {
    /// Sparse runtime overrides file (written by the SQL setters).
    runtime_path: PathBuf,
    /// Optional lower-precedence base config file (hand-edited / `P2P_CONFIG`).
    base_path: Option<PathBuf>,
    /// Directory for `0600` secret files (mnemonic / API key) — outside the repo.
    secrets_dir: PathBuf,
}

impl ConfigStore {
    /// Open the store using default locations:
    /// * runtime overrides: `$P2P_RUNTIME_CONFIG` or `<config-dir>/runtime.toml`,
    /// * base file: `$P2P_CONFIG` if set,
    /// * secrets: `<config-dir>/secrets/`.
    ///
    /// `<config-dir>` is `$P2P_CONFIG_DIR`, else `$XDG_CONFIG_HOME/duckdb-p2p`,
    /// else `$HOME/.config/duckdb-p2p` (or `%APPDATA%\duckdb-p2p` on Windows).
    pub fn open() -> Self {
        let dir = default_config_dir();
        let runtime_path = std::env::var("P2P_RUNTIME_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|_| dir.join("runtime.toml"));
        let base_path = std::env::var("P2P_CONFIG").ok().map(PathBuf::from);
        Self {
            runtime_path,
            base_path,
            secrets_dir: dir.join("secrets"),
        }
    }

    /// Construct a store with explicit paths (used by tests for hermeticity).
    pub fn with_paths(
        runtime_path: impl Into<PathBuf>,
        base_path: Option<PathBuf>,
        secrets_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            runtime_path: runtime_path.into(),
            base_path,
            secrets_dir: secrets_dir.into(),
        }
    }

    /// Path to the persisted runtime overrides file.
    pub fn runtime_path(&self) -> &Path {
        &self.runtime_path
    }

    /// Raw text of the runtime overrides file, if it exists (for inspection/tests).
    pub fn runtime_text(&self) -> Option<String> {
        std::fs::read_to_string(&self.runtime_path).ok()
    }

    // -- effective resolution ------------------------------------------------

    /// Resolve the effective config: defaults → base file → env → runtime layer.
    /// Lenient about the mainnet-confirm guard (so status can surface it); the
    /// guard is enforced on *changes* ([`ConfigStore::set`]) and on actions.
    pub fn effective(&self) -> Result<GridConfig, StoreError> {
        let base = GridConfig::load(self.base_path.as_deref())?;
        let runtime = self.read_runtime()?;
        let cfg = merge_runtime(&base, &runtime)?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn read_runtime(&self) -> Result<Table, StoreError> {
        match std::fs::read_to_string(&self.runtime_path) {
            Ok(text) => toml::from_str::<Table>(&text)
                .map_err(|e| StoreError::BadParam(format!("runtime config file is corrupt: {e}"))),
            Err(_) => Ok(Table::new()),
        }
    }

    // -- mutation ------------------------------------------------------------

    /// Apply a list of (dotted-key, value) overrides atomically: validate the
    /// resulting effective config, enforce the mainnet guard, then persist.
    /// Returns the new effective config. Nothing is written if validation fails.
    pub fn set(&self, pairs: &[(String, Value)]) -> Result<GridConfig, StoreError> {
        let mut runtime = self.read_runtime()?;
        for (path, val) in pairs {
            set_path(&mut runtime, path, val.clone());
        }

        let base = GridConfig::load(self.base_path.as_deref())?;
        let cfg = merge_runtime(&base, &runtime)?;
        cfg.validate()?;
        if cfg.economics.mainnet_blocked() {
            return Err(StoreError::Blocked(
                cfg.economics.guard_mainnet().unwrap_err(),
            ));
        }

        self.write_runtime(&runtime)?;
        Ok(cfg)
    }

    /// Generic escape hatch: set any config key by dotted path (`p2p_set`).
    /// The value is auto-typed (bool/int/float/else string).
    pub fn set_kv(&self, key: &str, value: &str) -> Result<GridConfig, StoreError> {
        self.set(&[(key.trim().to_string(), parse_scalar(value))])
    }

    /// Restore defaults by clearing the persisted runtime layer (`p2p_config_reset`).
    pub fn reset(&self) -> Result<(), StoreError> {
        match std::fs::remove_file(&self.runtime_path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(StoreError::Io(self.runtime_path.display().to_string(), e.to_string())),
        }
    }

    fn write_runtime(&self, runtime: &Table) -> Result<(), StoreError> {
        if let Some(parent) = self.runtime_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| StoreError::Io(parent.display().to_string(), e.to_string()))?;
        }
        let text = toml::to_string_pretty(&Value::Table(runtime.clone()))
            .map_err(|e| StoreError::BadParam(format!("serialize runtime config: {e}")))?;
        std::fs::write(&self.runtime_path, text)
            .map_err(|e| StoreError::Io(self.runtime_path.display().to_string(), e.to_string()))?;
        restrict_permissions(&self.runtime_path);
        Ok(())
    }

    // -- grouped, friendly setters ------------------------------------------

    /// Apply one of the friendly grouped setters by name. `params` maps the
    /// provided named arguments to their raw string values; absent args are
    /// simply not in the map. Returns the new effective config.
    ///
    /// Groups: `economics`, `trust`, `selection`, `fees`, `pricing`, `bidding`,
    /// `contracts`, `wallet`.
    pub fn apply_group(
        &self,
        group: &str,
        params: &BTreeMap<String, String>,
    ) -> Result<GridConfig, StoreError> {
        let pairs = self.group_pairs(group, params)?;
        if pairs.is_empty() {
            // Nothing supplied: just report the current effective config.
            return self.effective();
        }
        self.set(&pairs)
    }

    fn group_pairs(
        &self,
        group: &str,
        params: &BTreeMap<String, String>,
    ) -> Result<Vec<(String, Value)>, StoreError> {
        let mut out: Vec<(String, Value)> = Vec::new();
        let get = |k: &str| params.get(k).map(|s| s.as_str());

        match group {
            "economics" => {
                if let Some(v) = get("enabled") {
                    out.push(("economics.enabled".into(), parse_bool(v, "enabled")?));
                }
                if let Some(v) = get("settlement") {
                    out.push((
                        "economics.settlement".into(),
                        Value::String(friendly_settlement(v)?.to_string()),
                    ));
                }
                if let Some(v) = get("default_payment") {
                    validate_one_of("default_payment", v, &["free", "paid", "auto"])?;
                    out.push(("economics.default_payment".into(), Value::String(v.into())));
                }
                if let Some(v) = get("fee_recipient") {
                    out.push(("economics.fee_recipient".into(), Value::String(v.into())));
                }
                // Network switch (with the mainnet safety guard).
                let confirm = get("confirm").map(|v| parse_bool_raw(v, "confirm")).transpose()?;
                if let Some(net) = get("network") {
                    out.extend(network_change_pairs(net, confirm.unwrap_or(false))?);
                } else if let Some(c) = confirm {
                    // `confirm => true` on its own records the mainnet opt-in.
                    out.push(("economics.mainnet_confirmed".into(), Value::Boolean(c)));
                }
            }
            "trust" => {
                if let Some(v) = get("min_trust") {
                    out.push(("trust.min_trust".into(), parse_float(v, "min_trust")?));
                }
                if let Some(v) = get("min_attest").or_else(|| get("min_attestation")) {
                    let up = v.trim().to_uppercase();
                    validate_one_of("min_attest", &up, &["L0", "L1", "L2"])?;
                    out.push(("trust.min_attestation".into(), Value::String(up)));
                }
            }
            "selection" => {
                if let Some(v) = get("replicas") {
                    out.push(("scheduler.replicas".into(), parse_int(v, "replicas")?));
                }
                if let Some(v) = get("quorum") {
                    out.push(("scheduler.quorum".into(), parse_int(v, "quorum")?));
                }
                for k in ["checksum_min", "n_public", "n_default", "n_max"] {
                    if let Some(v) = get(k) {
                        out.push((format!("economics.selection.{k}"), parse_int(v, k)?));
                    }
                }
            }
            "fees" => {
                for k in [
                    "platform_fee_pct",
                    "participation_commission_frac",
                    "verification_surcharge_pct",
                    "bonus_aggressiveness",
                ] {
                    if let Some(v) = get(k) {
                        out.push((format!("economics.fees.{k}"), parse_float(v, k)?));
                    }
                }
            }
            "pricing" => {
                for k in ["unit_price", "max_bid"] {
                    if let Some(v) = get(k) {
                        out.push((format!("economics.pricing.{k}"), parse_int(v, k)?));
                    }
                }
            }
            "bidding" => {
                for k in ["w_quality", "w_stake", "w_price"] {
                    if let Some(v) = get(k) {
                        out.push((format!("economics.ranking.{k}"), parse_float(v, k)?));
                    }
                }
            }
            "contracts" => {
                let net = self.effective()?.economics.network.as_str().to_string();
                for k in ["stake_vault", "job_escrow", "record_anchor", "global_params"] {
                    if let Some(v) = get(k) {
                        out.push((format!("economics.{net}.contracts.{k}"), Value::String(v.into())));
                    }
                }
            }
            "wallet" => {
                let net = self.effective()?.economics.network.as_str().to_string();
                if let Some(v) = get("rpc") {
                    out.push((format!("economics.{net}.rpc"), Value::String(v.into())));
                }
                if let Some(v) = get("address") {
                    out.push((format!("economics.{net}.wallet.address"), Value::String(v.into())));
                }
                // Prefer file references; never persist a raw secret in the file.
                if let Some(v) = get("mnemonic_file") {
                    out.push((format!("economics.{net}.wallet.mnemonic_file"), Value::String(v.into())));
                } else if let Some(secret) = get("mnemonic") {
                    let path = self.store_secret(&format!("{net}.mnemonic"), secret)?;
                    out.push((
                        format!("economics.{net}.wallet.mnemonic_file"),
                        Value::String(path.display().to_string()),
                    ));
                }
                if let Some(v) = get("api_key_file") {
                    out.push((format!("economics.{net}.api_key_file"), Value::String(v.into())));
                } else if let Some(secret) = get("api_key") {
                    let path = self.store_secret(&format!("{net}.api_key"), secret)?;
                    out.push((
                        format!("economics.{net}.api_key_file"),
                        Value::String(path.display().to_string()),
                    ));
                }
            }
            other => {
                return Err(StoreError::BadParam(format!(
                    "unknown settings group '{other}' (economics|trust|selection|fees|pricing|\
                     bidding|contracts|wallet)"
                )))
            }
        }
        Ok(out)
    }

    // -- secrets -------------------------------------------------------------

    /// Persist a raw secret to a `0600` file under the secrets dir and return its
    /// path. The secret is NEVER written to the config file or echoed.
    pub fn store_secret(&self, name: &str, secret: &str) -> Result<PathBuf, StoreError> {
        std::fs::create_dir_all(&self.secrets_dir)
            .map_err(|e| StoreError::Io(self.secrets_dir.display().to_string(), e.to_string()))?;
        restrict_permissions(&self.secrets_dir);
        let safe: String = name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
            .collect();
        let path = self.secrets_dir.join(safe);
        std::fs::write(&path, secret)
            .map_err(|e| StoreError::Io(path.display().to_string(), e.to_string()))?;
        restrict_permissions(&path);
        Ok(path)
    }

    // -- inspection ----------------------------------------------------------

    /// Effective settings as grouped, human-readable rows with secrets redacted
    /// (`p2p_config()` / `p2p_settings()`).
    pub fn settings(&self) -> Result<Vec<SettingRow>, StoreError> {
        let cfg = self.effective()?;
        Ok(flatten_settings(&cfg))
    }

    /// A compact node/wallet/network/economics status summary (`p2p_status()`),
    /// prominently showing the **active network** + endpoints (secrets redacted).
    pub fn status(&self) -> Result<Vec<SettingRow>, StoreError> {
        Ok(status_rows(&self.effective()?))
    }
}

/// Build the `p2p_status()` rows from an already-resolved config (secrets
/// redacted, active network + endpoints prominent).
pub fn status_rows(cfg: &GridConfig) -> Vec<SettingRow> {
    {
        let e = &cfg.economics;
        let active = e.network.as_str();
        let payment = e.resolve_payment(crate::DataClassCfg::Public);
        let wallet = e.active_settings();
        let mut rows = vec![
            SettingRow::new("status", "network", active),
            SettingRow::new(
                "status",
                "network_confirmed",
                if matches!(e.network, TonNetwork::Mainnet) {
                    e.mainnet_confirmed.to_string()
                } else {
                    "n/a (testnet)".to_string()
                },
            ),
            SettingRow::new("status", "rpc_endpoint", e.resolved_rpc()),
            SettingRow::new("status", "explorer", e.resolved_explorer()),
            SettingRow::new("status", "economics_enabled", e.enabled.to_string()),
            SettingRow::new("status", "settlement", format!("{:?}", e.settlement).to_lowercase()),
            SettingRow::new("status", "default_payment", format!("{:?}", e.default_payment).to_lowercase()),
            SettingRow::new(
                "status",
                "public_jobs",
                if payment.is_paid() { "paid" } else { "free" },
            ),
            SettingRow::new(
                "status",
                "wallet_configured",
                wallet.wallet.mnemonic_file.is_some().to_string(),
            ),
            SettingRow::new(
                "status",
                "fee_recipient_set",
                e.fee_recipient.is_some().to_string(),
            ),
            SettingRow::new("status", "planner_prefer", format!("{:?}", cfg.planner.prefer).to_lowercase()),
        ];
        if e.mainnet_blocked() {
            rows.push(SettingRow::new(
                "status",
                "WARNING",
                "mainnet selected but NOT confirmed — paid/on-chain actions are blocked \
                 (real TON). Run p2p_economics(network => 'mainnet', confirm => true).",
            ));
        } else if matches!(e.network, TonNetwork::Mainnet) {
            rows.push(SettingRow::new(
                "status",
                "WARNING",
                "MAINNET active — real TON is at stake for paid/on-chain actions.",
            ));
        }
        rows
    }
}

// ---------------------------------------------------------------------------
// Free helpers (pure; unit-tested)
// ---------------------------------------------------------------------------

/// Map the business-friendly settlement name to the config enum value.
pub fn friendly_settlement(s: &str) -> Result<&'static str, StoreError> {
    match s.trim().to_ascii_lowercase().as_str() {
        "noop" | "free" | "off" => Ok("noop"),
        "mock" | "test" => Ok("mock"),
        "ton" | "onchain" | "on-chain" => Ok("onchain"),
        "channel" => Ok("channel"),
        other => Err(StoreError::BadParam(format!(
            "unknown settlement '{other}' (noop|mock|ton)"
        ))),
    }
}

/// Build the override pairs for a network change, enforcing the mainnet guard.
fn network_change_pairs(net: &str, confirm: bool) -> Result<Vec<(String, Value)>, StoreError> {
    match net.trim().to_ascii_lowercase().as_str() {
        "testnet" => Ok(vec![
            ("economics.network".into(), Value::String("testnet".into())),
            // Reset the opt-in when leaving mainnet, so a later switch re-confirms.
            ("economics.mainnet_confirmed".into(), Value::Boolean(false)),
        ]),
        "mainnet" => {
            if !confirm {
                return Err(StoreError::Blocked(
                    "switching to MAINNET puts REAL TON at stake. This requires an explicit \
                     opt-in: re-run `CALL p2p_economics(network => 'mainnet', confirm => true)`."
                        .into(),
                ));
            }
            Ok(vec![
                ("economics.network".into(), Value::String("mainnet".into())),
                ("economics.mainnet_confirmed".into(), Value::Boolean(true)),
            ])
        }
        other => Err(StoreError::BadParam(format!(
            "unknown network '{other}' (testnet|mainnet)"
        ))),
    }
}

fn validate_one_of(name: &str, v: &str, allowed: &[&str]) -> Result<(), StoreError> {
    if allowed.iter().any(|a| a.eq_ignore_ascii_case(v.trim())) {
        Ok(())
    } else {
        Err(StoreError::BadParam(format!(
            "{name} must be one of [{}], got '{v}'",
            allowed.join("|")
        )))
    }
}

fn parse_bool(v: &str, name: &str) -> Result<Value, StoreError> {
    Ok(Value::Boolean(parse_bool_raw(v, name)?))
}

fn parse_bool_raw(v: &str, name: &str) -> Result<bool, StoreError> {
    match v.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        other => Err(StoreError::BadParam(format!("{name} must be true/false, got '{other}'"))),
    }
}

fn parse_int(v: &str, name: &str) -> Result<Value, StoreError> {
    v.trim()
        .parse::<i64>()
        .map(Value::Integer)
        .map_err(|_| StoreError::BadParam(format!("{name} must be an integer, got '{v}'")))
}

fn parse_float(v: &str, name: &str) -> Result<Value, StoreError> {
    v.trim()
        .parse::<f64>()
        .map(Value::Float)
        .map_err(|_| StoreError::BadParam(format!("{name} must be a number, got '{v}'")))
}

/// Auto-type a raw string for the generic `p2p_set`: bool, then int, then float,
/// else string.
fn parse_scalar(s: &str) -> Value {
    let t = s.trim();
    if let "true" | "false" = t.to_ascii_lowercase().as_str() {
        return Value::Boolean(t.eq_ignore_ascii_case("true"));
    }
    if let Ok(i) = t.parse::<i64>() {
        return Value::Integer(i);
    }
    if let Ok(f) = t.parse::<f64>() {
        return Value::Float(f);
    }
    Value::String(s.to_string())
}

/// Merge the sparse runtime overrides on top of a fully-resolved base config and
/// deserialize back to a typed [`GridConfig`] (so unknown keys / wrong types are
/// rejected with a friendly message via `deny_unknown_fields`).
fn merge_runtime(base: &GridConfig, runtime: &Table) -> Result<GridConfig, StoreError> {
    let mut base_val = Value::try_from(base)
        .map_err(|e| StoreError::BadParam(format!("internal: serialize base config: {e}")))?;
    deep_merge(&mut base_val, &Value::Table(runtime.clone()));
    base_val
        .try_into::<GridConfig>()
        .map_err(|e| StoreError::BadParam(friendly_deser_error(&e.to_string())))
}

fn friendly_deser_error(raw: &str) -> String {
    if raw.contains("unknown field") {
        format!("{raw} — check the key path (see `SELECT * FROM p2p_config()` for valid keys)")
    } else {
        format!("rejected: {raw}")
    }
}

fn set_path(table: &mut Table, dotted: &str, val: Value) {
    let parts: Vec<&str> = dotted.split('.').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return;
    }
    set_path_inner(table, &parts, val);
}

fn set_path_inner(table: &mut Table, parts: &[&str], val: Value) {
    let (head, rest) = parts.split_first().expect("non-empty");
    if rest.is_empty() {
        table.insert((*head).to_string(), val);
        return;
    }
    let is_table = table.get(*head).map(Value::is_table).unwrap_or(false);
    if !is_table {
        table.insert((*head).to_string(), Value::Table(Table::new()));
    }
    let child = table
        .get_mut(*head)
        .and_then(Value::as_table_mut)
        .expect("just ensured table");
    set_path_inner(child, rest, val);
}

fn deep_merge(base: &mut Value, overlay: &Value) {
    match (base, overlay) {
        (Value::Table(b), Value::Table(o)) => {
            for (k, ov) in o.iter() {
                let merge_into_child = b.get(k).map(Value::is_table).unwrap_or(false) && ov.is_table();
                if merge_into_child {
                    deep_merge(b.get_mut(k).unwrap(), ov);
                } else {
                    b.insert(k.clone(), ov.clone());
                }
            }
        }
        (b, o) => *b = o.clone(),
    }
}

/// Flatten the effective config into grouped rows, redacting secret-named fields.
pub fn flatten_settings(cfg: &GridConfig) -> Vec<SettingRow> {
    let v = match Value::try_from(cfg) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let mut rows = Vec::new();
    walk("", &v, &mut rows);
    rows
}

fn walk(prefix: &str, v: &Value, rows: &mut Vec<SettingRow>) {
    match v {
        Value::Table(t) => {
            for (k, child) in t.iter() {
                let p = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                walk(&p, child, rows);
            }
        }
        leaf => {
            let (group, key) = match prefix.split_once('.') {
                Some((g, rest)) => (g.to_string(), rest.to_string()),
                None => (prefix.to_string(), prefix.to_string()),
            };
            let last = prefix.rsplit('.').next().unwrap_or(prefix);
            let value = if is_secret_key(last) {
                "<redacted>".to_string()
            } else {
                display_value(leaf)
            };
            rows.push(SettingRow::new(group, key, value));
        }
    }
}

fn display_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Integer(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::Array(a) => a.iter().map(display_value).collect::<Vec<_>>().join(", "),
        other => other.to_string(),
    }
}

/// Whether a (leaf) key name denotes a raw secret that must never be displayed.
/// Note the config only ever stores `*_file` references, which are NOT secrets.
fn is_secret_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    matches!(
        k.as_str(),
        "mnemonic" | "api_key" | "apikey" | "secret" | "private_key" | "privatekey" | "seed" | "passphrase"
    )
}

fn default_config_dir() -> PathBuf {
    if let Ok(d) = std::env::var("P2P_CONFIG_DIR") {
        return PathBuf::from(d);
    }
    if let Ok(x) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(x).join("duckdb-p2p");
    }
    if let Ok(h) = std::env::var("HOME") {
        if !h.is_empty() {
            return PathBuf::from(h).join(".config").join("duckdb-p2p");
        }
    }
    if let Ok(a) = std::env::var("APPDATA") {
        return PathBuf::from(a).join("duckdb-p2p");
    }
    std::env::temp_dir().join("duckdb-p2p")
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mode = if meta.is_dir() { 0o700 } else { 0o600 };
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (ConfigStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigStore::with_paths(
            dir.path().join("runtime.toml"),
            None,
            dir.path().join("secrets"),
        );
        (store, dir)
    }

    #[test]
    fn zero_config_effective_equals_defaults() {
        let (store, _d) = temp_store();
        assert_eq!(store.effective().unwrap(), GridConfig::default());
        assert!(store.runtime_text().is_none(), "nothing persisted yet");
    }

    #[test]
    fn set_is_reflected_and_persisted() {
        let (store, dir) = temp_store();
        let cfg = store
            .set(&[("trust.min_trust".into(), Value::Float(0.9))])
            .unwrap();
        assert_eq!(cfg.trust.min_trust, 0.9);
        // Survives a reopen (persisted runtime layer).
        let reopened = ConfigStore::with_paths(
            store.runtime_path().to_path_buf(),
            None,
            dir.path().join("secrets"),
        );
        assert_eq!(reopened.effective().unwrap().trust.min_trust, 0.9);
        assert!(store.runtime_text().unwrap().contains("min_trust"));
    }

    #[test]
    fn invalid_value_rejected_with_friendly_message_and_not_persisted() {
        let (store, _d) = temp_store();
        // quorum > replicas is a cross-field invariant violation.
        let err = store
            .set(&[("scheduler.quorum".into(), Value::Integer(99))])
            .unwrap_err();
        assert!(format!("{err}").to_lowercase().contains("quorum"), "got {err}");
        assert!(store.runtime_text().is_none(), "must not persist on failure");
    }

    #[test]
    fn unknown_key_rejected() {
        let (store, _d) = temp_store();
        let err = store.set_kv("economics.bogus_key", "1").unwrap_err();
        assert!(format!("{err}").contains("unknown field"), "got {err}");
    }

    #[test]
    fn default_network_is_testnet() {
        let (store, _d) = temp_store();
        assert_eq!(store.effective().unwrap().economics.network, TonNetwork::Testnet);
    }

    #[test]
    fn mainnet_requires_confirm() {
        let (store, _d) = temp_store();
        let mut p = BTreeMap::new();
        p.insert("network".to_string(), "mainnet".to_string());
        let err = store.apply_group("economics", &p).unwrap_err();
        assert!(format!("{err}").to_lowercase().contains("real ton"), "got {err}");
        // Nothing persisted.
        assert!(store.runtime_text().is_none());

        // With confirm => true it switches and records the opt-in.
        p.insert("confirm".to_string(), "true".to_string());
        let cfg = store.apply_group("economics", &p).unwrap();
        assert_eq!(cfg.economics.network, TonNetwork::Mainnet);
        assert!(cfg.economics.mainnet_confirmed);
    }

    #[test]
    fn per_network_addresses_resolve_on_switch() {
        let (store, _d) = temp_store();
        // Register testnet contract while on testnet (the active net).
        let mut c = BTreeMap::new();
        c.insert("stake_vault".to_string(), "kQtestVault".to_string());
        store.apply_group("contracts", &c).unwrap();

        // Switch to mainnet (confirmed) and register a different address.
        let mut e = BTreeMap::new();
        e.insert("network".to_string(), "mainnet".to_string());
        e.insert("confirm".to_string(), "true".to_string());
        store.apply_group("economics", &e).unwrap();
        let mut c2 = BTreeMap::new();
        c2.insert("stake_vault".to_string(), "kQmainVault".to_string());
        let cfg = store.apply_group("contracts", &c2).unwrap();

        // Active (mainnet) resolves to the mainnet address + endpoints.
        assert_eq!(cfg.economics.active_settings().contracts.stake_vault.as_deref(), Some("kQmainVault"));
        assert_eq!(cfg.economics.resolved_rpc(), "https://toncenter.com/api/v2/");
        // Testnet address is retained simultaneously.
        assert_eq!(cfg.economics.testnet.contracts.stake_vault.as_deref(), Some("kQtestVault"));
    }

    #[test]
    fn wallet_secret_never_in_config_or_file() {
        let (store, _d) = temp_store();
        let mut w = BTreeMap::new();
        w.insert("mnemonic".to_string(), "abandon abandon secret words".to_string());
        let cfg = store.apply_group("wallet", &w).unwrap();
        // Config stores only a file reference.
        let mref = cfg.economics.testnet.wallet.mnemonic_file.clone().unwrap();
        assert!(!mref.contains("abandon"));
        // The raw secret never lands in the runtime file...
        assert!(!store.runtime_text().unwrap().contains("abandon"));
        // ...and is redacted from settings output.
        let rows = store.settings().unwrap();
        assert!(rows.iter().all(|r| !r.value.contains("abandon")));
    }

    #[test]
    fn status_shows_active_network() {
        let (store, _d) = temp_store();
        let rows = store.status().unwrap();
        let net = rows.iter().find(|r| r.key == "network").unwrap();
        assert_eq!(net.value, "testnet");
        assert!(rows.iter().any(|r| r.key == "rpc_endpoint" && r.value.contains("testnet.toncenter.com")));
    }

    #[test]
    fn reset_restores_defaults() {
        let (store, _d) = temp_store();
        store.set_kv("trust.min_trust", "0.95").unwrap();
        assert_eq!(store.effective().unwrap().trust.min_trust, 0.95);
        store.reset().unwrap();
        assert_eq!(store.effective().unwrap(), GridConfig::default());
    }

    #[test]
    fn friendly_settlement_maps_ton_to_onchain() {
        assert_eq!(friendly_settlement("ton").unwrap(), "onchain");
        assert_eq!(friendly_settlement("mock").unwrap(), "mock");
        assert_eq!(friendly_settlement("noop").unwrap(), "noop");
        assert!(friendly_settlement("dogecoin").is_err());
    }
}
