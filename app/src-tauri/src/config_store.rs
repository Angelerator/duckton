//! On-disk state for the Duckton Node app.
//!
//! The canonical config is persisted as `config.json` in the OS app-config dir
//! (robust round-tripping of the full [`GridConfig`]). A best-effort `p2p.toml`
//! mirror is also written so the same node config can be reused by the CLI /
//! `duckton` extension. Secrets (wallet mnemonics, toncenter API keys) are NEVER
//! stored in the config: they go to `0600` files under `secrets/`, and only the
//! file *path* is referenced from the config — matching the repo convention
//! (`economics.<net>.wallet.mnemonic_file`, `api_key_file`).

use std::path::PathBuf;

use anyhow::{Context, Result};
use p2p_config::GridConfig;

/// Resolved on-disk locations for the app's state.
#[derive(Clone, Debug)]
pub struct Paths {
    pub root: PathBuf,
    pub config_file: PathBuf,
    pub toml_export: PathBuf,
    pub secrets_dir: PathBuf,
    pub identity_key: PathBuf,
}

impl Paths {
    pub fn new(app_config_dir: PathBuf) -> Self {
        Self {
            config_file: app_config_dir.join("config.json"),
            toml_export: app_config_dir.join("p2p.toml"),
            secrets_dir: app_config_dir.join("secrets"),
            identity_key: app_config_dir.join("node.key"),
            root: app_config_dir,
        }
    }

    /// Create the state + secrets directories (idempotent) with owner-only perms.
    pub fn ensure(&self) -> Result<()> {
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("create config dir {}", self.root.display()))?;
        std::fs::create_dir_all(&self.secrets_dir)
            .with_context(|| format!("create secrets dir {}", self.secrets_dir.display()))?;
        set_dir_private(&self.secrets_dir);
        Ok(())
    }
}

/// Load the persisted config, or the built-in defaults seeded for a host node on
/// first run.
pub fn load_config(paths: &Paths) -> Result<GridConfig> {
    if paths.config_file.exists() {
        let text = std::fs::read_to_string(&paths.config_file)
            .with_context(|| format!("read {}", paths.config_file.display()))?;
        let mut cfg: GridConfig = serde_json::from_str(&text).context("parse config.json")?;
        // Self-heal a persisted bind address that isn't a valid IP:port (e.g. a
        // seed host accidentally entered in the bind field) so a bad value can't
        // permanently wedge startup — fall back to the host default and log it.
        if cfg.network.bind_addr.parse::<std::net::SocketAddr>().is_err() {
            tracing::warn!(
                bad = %cfg.network.bind_addr,
                "persisted QUIC bind address is not a valid IP:port; falling back to 0.0.0.0:9494"
            );
            cfg.network.bind_addr = "0.0.0.0:9494".to_string();
        }
        Ok(cfg)
    } else {
        Ok(default_node_config())
    }
}

/// Persist the config (canonical JSON + best-effort TOML mirror).
pub fn save_config(paths: &Paths, cfg: &GridConfig) -> Result<()> {
    paths.ensure()?;
    let json = serde_json::to_string_pretty(cfg).context("serialize config.json")?;
    std::fs::write(&paths.config_file, json)
        .with_context(|| format!("write {}", paths.config_file.display()))?;
    set_file_private(&paths.config_file);
    // Best-effort TOML mirror for reuse by the CLI / `duckton` extension. Never
    // fail the save on a TOML serialization quirk — JSON is the source of truth.
    if let Ok(toml) = toml::to_string_pretty(cfg) {
        let _ = std::fs::write(&paths.toml_export, toml);
    }
    Ok(())
}

/// Write a secret (mnemonic / API key) to a `0600` file and return its path. The
/// secret value itself never touches the config or the webview persistence.
pub fn write_secret(paths: &Paths, name: &str, contents: &str) -> Result<PathBuf> {
    paths.ensure()?;
    let path = paths.secrets_dir.join(name);
    std::fs::write(&path, contents.trim())
        .with_context(|| format!("write secret {}", path.display()))?;
    set_file_private(&path);
    Ok(path)
}

/// Ensure a stable Ed25519 node identity exists on disk (so the node keeps the
/// same `NodeId` — and its reputation — across restarts). Returns the key path.
pub fn ensure_identity_key(paths: &Paths) -> Result<PathBuf> {
    paths.ensure()?;
    if !paths.identity_key.exists() {
        let identity = p2p_transport::NodeIdentity::generate()
            .map_err(|e| anyhow::anyhow!("generate identity: {e}"))?;
        let pem = identity
            .to_pem()
            .map_err(|e| anyhow::anyhow!("encode identity: {e}"))?;
        std::fs::write(&paths.identity_key, pem.as_str())
            .with_context(|| format!("write {}", paths.identity_key.display()))?;
        set_file_private(&paths.identity_key);
    }
    Ok(paths.identity_key.clone())
}

/// First-run defaults tuned for a **pure host node** (introduce yourself as a
/// node and serve the grid):
///  * bind on all interfaces at the conventional QUIC port (reachable host),
///  * local self-execution OFF (this machine never runs its OWN queries — it is
///    purely a host, so the whole donated budget goes to serving),
///  * the Duckton public seed pre-filled so it can reach the grid out of the box,
///  * one-shot self-profile collection (no hourly refresh): the periodic task
///    holds the live QUIC transport for its whole lifetime, which would pin the
///    listen socket open across an in-app node restart. A desktop node's machine
///    profile is effectively static, so a single collection on start is enough.
fn default_node_config() -> GridConfig {
    let mut cfg = GridConfig::default();
    cfg.network.bind_addr = "0.0.0.0:9494".to_string();
    cfg.planner.local_execution_enabled = false;
    cfg.discovery.bootstrap = vec!["quic://seed.duckton.com:9494".to_string()];
    cfg.metadata.refresh_interval_secs = 0;
    cfg
}

#[cfg(unix)]
fn set_file_private(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(unix)]
fn set_dir_private(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn set_file_private(_path: &std::path::Path) {}

#[cfg(not(unix))]
fn set_dir_private(_path: &std::path::Path) {}
