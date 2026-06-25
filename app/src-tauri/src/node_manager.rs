//! Embeds the Duckton grid core (`p2p-node`) as a long-lived **host**: it builds
//! a [`Node`] from the persisted [`GridConfig`], wires the configured TON
//! settlement rail, and spawns the worker accept loop so the machine serves grid
//! jobs. All lifecycle + on-chain actions the GUI triggers live here.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use tokio::sync::Mutex;

use p2p_config::{
    DataClassCfg, GridConfig, PaymentPref, PinningMode, SecurityMode, SettlementRail, TonNetwork,
};
use p2p_node::{DuckDbEngine, EngineError, ExecLease, JobContext, Node, QueryEngine, StorageSetup};
use p2p_proto::ResultSet;

use crate::config_store::{self, Paths};
use crate::dto::{ConfigView, NodeStatus};

/// Shared, in-process log ring buffer (also fed by a tracing writer).
pub type LogBuffer = Arc<StdMutex<VecDeque<String>>>;

/// Build the locked-down host engine. Identical to [`DuckDbEngine::new`]
/// ([`StorageSetup::strict`]: no remote egress, no allowed dirs, no providers)
/// but with on-disk spill encryption disabled. The host preloads no `httpfs`
/// crypto provider, so DuckDB cannot encrypt spill at rest anyway and would just
/// log a warning and self-downgrade `temp_file_encryption` on every engine init.
/// Turning it off up front keeps the exact same behavior without the noise.
fn build_host_engine() -> Result<DuckDbEngine, EngineError> {
    let mut setup = StorageSetup::strict();
    setup.temp_file_encryption = false;
    DuckDbEngine::with_setup(setup)
}

/// A [`QueryEngine`] decorator that counts executions so the dashboard can show
/// "jobs served" (foreign jobs via `execute_job*`) vs local self-runs.
pub struct CountingEngine {
    inner: Arc<dyn QueryEngine>,
    served: Arc<AtomicU64>,
    local: Arc<AtomicU64>,
}

#[async_trait]
impl QueryEngine for CountingEngine {
    async fn execute(&self, sql: &str, lease: ExecLease) -> Result<ResultSet, EngineError> {
        self.local.fetch_add(1, Ordering::Relaxed);
        self.inner.execute(sql, lease).await
    }

    async fn execute_job(
        &self,
        sql: &str,
        lease: ExecLease,
        ctx: &JobContext,
    ) -> Result<ResultSet, EngineError> {
        self.served.fetch_add(1, Ordering::Relaxed);
        self.inner.execute_job(sql, lease, ctx).await
    }

    async fn execute_job_cancellable(
        &self,
        sql: &str,
        lease: ExecLease,
        ctx: &JobContext,
        cancel: Arc<tokio::sync::Notify>,
    ) -> Result<ResultSet, EngineError> {
        self.served.fetch_add(1, Ordering::Relaxed);
        self.inner
            .execute_job_cancellable(sql, lease, ctx, cancel)
            .await
    }

    fn version(&self) -> String {
        self.inner.version()
    }
}

/// A running node + its background tasks.
struct NodeRuntime {
    _node: Node,
    host_task: tokio::task::JoinHandle<()>,
    params_sync: tokio::task::JoinHandle<()>,
    node_id: String,
    listen_addr: String,
    started_at: Instant,
}

impl Drop for NodeRuntime {
    fn drop(&mut self) {
        self.host_task.abort();
        self.params_sync.abort();
    }
}

/// Tauri-managed application state.
pub struct AppState {
    pub paths: Paths,
    pub config: Mutex<GridConfig>,
    runtime: Mutex<Option<NodeRuntime>>,
    served: Arc<AtomicU64>,
    local: Arc<AtomicU64>,
    /// DuckDB library version, probed once at startup (the host engine version).
    engine_version: String,
    pub logs: LogBuffer,
}

impl AppState {
    pub fn new(paths: Paths, config: GridConfig, logs: LogBuffer) -> Self {
        let engine_version = build_host_engine()
            .map(|e| e.version())
            .unwrap_or_default();
        Self {
            paths,
            config: Mutex::new(config),
            runtime: Mutex::new(None),
            served: Arc::new(AtomicU64::new(0)),
            local: Arc::new(AtomicU64::new(0)),
            engine_version,
            logs,
        }
    }

    /// Persist the current in-memory config to disk.
    pub async fn persist(&self) -> Result<()> {
        let cfg = self.config.lock().await.clone();
        config_store::save_config(&self.paths, &cfg)
    }

    /// Build + start the host node from the current config. Idempotent: if a node
    /// is already running it is stopped and replaced (apply config changes).
    pub async fn start(&self) -> Result<NodeStatus> {
        self.stop().await;

        let mut cfg = self.config.lock().await.clone();
        // Pin a stable on-disk identity so the node keeps its NodeId/reputation.
        let key = config_store::ensure_identity_key(&self.paths)?;
        cfg.identity.key_path = Some(key.to_string_lossy().into_owned());
        cfg.validate().map_err(|e| anyhow!("invalid config: {e}"))?;

        // Real locked-down, bundled DuckDB engine, wrapped to count served jobs.
        let inner: Arc<dyn QueryEngine> =
            Arc::new(build_host_engine().map_err(|e| anyhow!("engine init: {e}"))?);
        let engine_version = inner.version();
        let engine: Arc<dyn QueryEngine> = Arc::new(CountingEngine {
            inner,
            served: Arc::clone(&self.served),
            local: Arc::clone(&self.local),
        });

        // Bind the node's QUIC endpoint. Right after a stop()/restart the previous
        // endpoint's UDP socket can take a brief moment to be released by the async
        // driver, so a back-to-back "Save & Restart" may transiently observe
        // "address already in use" — retry the bind a few times (≈1s budget)
        // before surfacing the error rather than failing the restart outright.
        let mut node = {
            let mut attempt = 0u32;
            loop {
                match Node::with_config(cfg.clone(), Arc::clone(&engine)) {
                    Ok(node) => break node,
                    Err(e) => {
                        let msg = e.to_string();
                        if attempt < 20 && msg.to_ascii_lowercase().contains("in use") {
                            attempt += 1;
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            continue;
                        }
                        return Err(anyhow!("build node: {msg}"));
                    }
                }
            }
        };

        // Wire the configured settlement rail (free/noop, mock, or live on-chain
        // for testnet/mainnet). `resolve_settlement_stack` fails closed on an
        // unconfirmed mainnet, so a misconfigured paid node never starts.
        let stack = p2p_settlement::resolve_settlement_stack(&cfg.economics)
            .map_err(|e| anyhow!("settlement: {e}"))?;
        node = node.with_settlement_stack(stack);

        let node_id = node.node_id().0.clone();
        let listen_addr = node
            .local_addr()
            .map_err(|e| anyhow!("listen addr: {e}"))?
            .to_string();

        // Periodic on-chain GlobalParams policy sync (a no-op unless a params
        // source is wired by the on-chain rail).
        let params_sync = node.spawn_params_sync(Duration::from_secs(300));
        let host_task = node.spawn_host();

        self.served.store(0, Ordering::Relaxed);
        self.local.store(0, Ordering::Relaxed);

        tracing::info!(node_id = %node_id, listen = %listen_addr, engine = %engine_version, "host node started");

        *self.runtime.lock().await = Some(NodeRuntime {
            _node: node,
            host_task,
            params_sync,
            node_id,
            listen_addr,
            started_at: Instant::now(),
        });

        self.status().await
    }

    /// Stop the host node: cancel the accept loop + background tasks and wait for
    /// them to fully unwind so they drop their clones of the QUIC transport, then
    /// drop the node (the last transport holder, since metadata collection is
    /// one-shot). This frees the bound UDP port before any subsequent restart.
    pub async fn stop(&self) {
        // Take the runtime out under the lock, then release the lock before
        // awaiting teardown (so concurrent status() polls are never blocked).
        let rt = self.runtime.lock().await.take();
        if let Some(mut rt) = rt {
            rt.host_task.abort();
            rt.params_sync.abort();
            // JoinHandle is Unpin, so `&mut handle` is itself a Future; awaiting it
            // returns once the cancelled task has finished and dropped its captures.
            let _ = (&mut rt.host_task).await;
            let _ = (&mut rt.params_sync).await;
            drop(rt);
            tracing::info!("host node stopped");
        }
    }

    /// Current live + configured status.
    pub async fn status(&self) -> Result<NodeStatus> {
        let cfg = self.config.lock().await.clone();
        let rt = self.runtime.lock().await;
        let econ = &cfg.economics;
        let settings = econ.active_settings();

        let mut s = NodeStatus {
            running: rt.is_some(),
            jobs_served: self.served.load(Ordering::Relaxed),
            local_jobs: self.local.load(Ordering::Relaxed),
            engine_version: self.engine_version.clone(),
            protocol_version: cfg.protocol.version.clone(),
            network: econ.network.as_str().to_string(),
            economics_enabled: econ.enabled,
            settlement: settlement_rail_str(econ.settlement).to_string(),
            mainnet_confirmed: econ.mainnet_confirmed,
            default_payment: payment_pref_str(econ.default_payment).to_string(),
            wallet_address: settings.wallet.address.clone(),
            memory_bytes: cfg.budget.memory_bytes,
            threads: cfg.budget.threads,
            max_jobs: cfg.budget.max_jobs,
            data_classes: cfg
                .budget
                .data_classes
                .iter()
                .map(|d| data_class_str(*d).to_string())
                .collect(),
            bootstrap: cfg.discovery.bootstrap.clone(),
            bind_addr: cfg.network.bind_addr.clone(),
            unit_price: econ.pricing.unit_price,
            max_bid: econ.pricing.max_bid,
            fee_recipient: econ.fee_recipient.clone(),
            stake_vault: settings.contracts.stake_vault.clone(),
            global_params: settings.contracts.global_params.clone(),
            job_escrow: settings.contracts.job_escrow.clone(),
            record_anchor: settings.contracts.record_anchor.clone(),
            rpc_endpoint: econ.resolved_rpc(),
            explorer: resolved_explorer(econ),
            ..Default::default()
        };
        if let Some(rt) = rt.as_ref() {
            s.node_id = Some(rt.node_id.clone());
            s.listen_addr = Some(rt.listen_addr.clone());
            s.uptime_secs = rt.started_at.elapsed().as_secs();
        }
        Ok(s)
    }

    /// Broadcast an on-chain `StakeDeposit` (`action = "stake"`) or
    /// `StakeRequestUnbond` (`"unstake"`) from the configured wallet — the same
    /// flow the `duckton` extension's `p2p_stake`/`p2p_unstake` drive.
    pub async fn stake_action(&self, action: &str, amount_ton: u64) -> Result<String> {
        let econ = self.config.lock().await.economics.clone();
        if amount_ton == 0 {
            bail!("amount must be a positive whole-TON value");
        }
        if !econ.enabled
            || !matches!(econ.settlement, SettlementRail::Onchain | SettlementRail::Channel)
        {
            bail!("staking requires on-chain settlement — enable economics with settlement = 'ton' first");
        }
        econ.guard_mainnet().map_err(|e| anyhow!(e))?;
        let settings = econ.active_settings();
        let vault = settings
            .contracts
            .stake_vault
            .clone()
            .ok_or_else(|| anyhow!("no stake_vault contract set for this network"))?;
        let mnemonic_path = settings
            .wallet
            .mnemonic_file
            .clone()
            .ok_or_else(|| anyhow!("no wallet configured for this network"))?;

        let action = action.to_string();
        // The toncenter client shells out (curl) and blocks — keep it off the
        // async reactor.
        let tx = tokio::task::spawn_blocking(move || -> Result<String> {
            use p2p_settlement::{TonRpc, WalletAddress};
            let wiring = p2p_settlement::resolve_ton_wiring(&econ).map_err(|e| anyhow!("{e}"))?;
            let mnemonic = std::fs::read_to_string(&mnemonic_path)
                .with_context(|| format!("read mnemonic file {mnemonic_path}"))?;
            let rpc = p2p_settlement::ToncenterRpc::new(
                &wiring.rpc_endpoint,
                wiring.api_key.clone(),
                &wiring.network,
                mnemonic.trim(),
            )
            .map_err(|e| anyhow!("rpc: {e}"))?;
            let vault_addr =
                WalletAddress::from_any_str(&vault).map_err(|_| anyhow!("malformed stake_vault address"))?;
            const TON: u128 = 1_000_000_000;
            const GAS: u128 = 100_000_000; // 0.1 TON compute/storage headroom
            let amt = (amount_ton as u128) * TON;
            let qid = now_secs();
            let hash = if action == "unstake" {
                rpc.send_internal(&vault_addr, GAS, &p2p_settlement::build_stake_unbond(qid, amt))
                    .map_err(|e| anyhow!("{e}"))?
            } else {
                rpc.send_internal(
                    &vault_addr,
                    amt + GAS,
                    &p2p_settlement::build_stake_deposit(qid, amt),
                )
                .map_err(|e| anyhow!("{e}"))?
            };
            Ok(hash)
        })
        .await
        .context("stake task join")??;

        Ok(tx)
    }

    /// Apply the editable host config from the Configuration screen onto the
    /// in-memory `GridConfig` (preserving advanced fields), then persist.
    pub async fn apply_config(&self, view: ConfigView) -> Result<()> {
        {
            let mut cfg = self.config.lock().await;
            // The QUIC bind address must be a concrete IP:port (it is parsed as a
            // `SocketAddr` when the endpoint binds). Reject anything else up front
            // so a stray hostname — e.g. the Duckton seed mistakenly entered here
            // instead of in Bootstrap seeds — can't be persisted and wedge startup.
            let bind = view.bind_addr.trim();
            if bind.parse::<std::net::SocketAddr>().is_err() {
                bail!(
                    "QUIC bind address must be an IP:port such as 0.0.0.0:9494 (got '{bind}'). \
                     Tip: a seed host like seed.duckton.com:9494 belongs in the Bootstrap seeds \
                     field, not the bind address."
                );
            }
            cfg.network.bind_addr = bind.to_string();
            cfg.network.advertised_addr = view
                .advertised_addr
                .and_then(|a| {
                    let a = a.trim().to_string();
                    if a.is_empty() { None } else { Some(a) }
                });
            cfg.budget.memory_bytes = view.memory_bytes;
            cfg.budget.threads = view.threads;
            cfg.budget.max_jobs = view.max_jobs;
            cfg.budget.per_job_memory_bytes = view.per_job_memory_bytes;
            cfg.budget.data_classes =
                view.data_classes.iter().map(|s| parse_data_class(s)).collect();
            cfg.discovery.bootstrap = view
                .bootstrap
                .into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            cfg.identity.pinning_mode = parse_pinning(&view.pinning_mode);
            cfg.security.mode = parse_security(&view.security_mode);
            cfg.discovery.nat.mdns = view.mdns;
            cfg.discovery.nat.autonat = view.autonat;
            cfg.discovery.nat.relay_client = view.relay_client;
            cfg.discovery.nat.act_as_relay = view.act_as_relay;
            // Validate before we persist so the UI gets immediate feedback.
            cfg.validate().map_err(|e| anyhow!("invalid config: {e}"))?;
        }
        self.persist().await
    }

    /// Read the editable host config for the Configuration screen.
    pub async fn config_view(&self) -> ConfigView {
        let cfg = self.config.lock().await;
        ConfigView {
            bind_addr: cfg.network.bind_addr.clone(),
            advertised_addr: cfg.network.advertised_addr.clone(),
            memory_bytes: cfg.budget.memory_bytes,
            threads: cfg.budget.threads,
            max_jobs: cfg.budget.max_jobs,
            per_job_memory_bytes: cfg.budget.per_job_memory_bytes,
            data_classes: cfg
                .budget
                .data_classes
                .iter()
                .map(|d| data_class_str(*d).to_string())
                .collect(),
            bootstrap: cfg.discovery.bootstrap.clone(),
            pinning_mode: pinning_str(cfg.identity.pinning_mode).to_string(),
            security_mode: security_str(cfg.security.mode).to_string(),
            mdns: cfg.discovery.nat.mdns,
            autonat: cfg.discovery.nat.autonat,
            relay_client: cfg.discovery.nat.relay_client,
            act_as_relay: cfg.discovery.nat.act_as_relay,
        }
    }
}

// --------------------------------------------------------------------------
// enum <-> string mapping helpers (the UI speaks strings)
// --------------------------------------------------------------------------

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn parse_data_class(s: &str) -> DataClassCfg {
    match s.trim().to_ascii_lowercase().as_str() {
        "internal" => DataClassCfg::Internal,
        "sensitive" => DataClassCfg::Sensitive,
        _ => DataClassCfg::Public,
    }
}

fn data_class_str(d: DataClassCfg) -> &'static str {
    match d {
        DataClassCfg::Public => "public",
        DataClassCfg::Internal => "internal",
        DataClassCfg::Sensitive => "sensitive",
    }
}

pub fn parse_pinning(s: &str) -> PinningMode {
    match s.trim().to_ascii_lowercase().as_str() {
        "allowlist" => PinningMode::Allowlist,
        _ => PinningMode::Tofu,
    }
}

fn pinning_str(p: PinningMode) -> &'static str {
    match p {
        PinningMode::Tofu => "tofu",
        PinningMode::Allowlist => "allowlist",
    }
}

pub fn parse_security(s: &str) -> SecurityMode {
    match s.trim().to_ascii_lowercase().as_str() {
        "private" => SecurityMode::Private,
        _ => SecurityMode::Public,
    }
}

fn security_str(m: SecurityMode) -> &'static str {
    match m {
        SecurityMode::Public => "public",
        SecurityMode::Private => "private",
    }
}

pub fn parse_settlement(s: &str) -> SettlementRail {
    match s.trim().to_ascii_lowercase().as_str() {
        "ton" | "onchain" => SettlementRail::Onchain,
        "channel" => SettlementRail::Channel,
        "mock" => SettlementRail::Mock,
        _ => SettlementRail::Noop,
    }
}

fn settlement_rail_str(r: SettlementRail) -> &'static str {
    match r {
        SettlementRail::Noop => "noop",
        SettlementRail::Mock => "mock",
        SettlementRail::Onchain => "ton",
        SettlementRail::Channel => "channel",
    }
}

pub fn parse_network(s: &str) -> TonNetwork {
    match s.trim().to_ascii_lowercase().as_str() {
        "mainnet" => TonNetwork::Mainnet,
        _ => TonNetwork::Testnet,
    }
}

pub fn parse_payment(s: &str) -> PaymentPref {
    match s.trim().to_ascii_lowercase().as_str() {
        "free" => PaymentPref::Free,
        "paid" => PaymentPref::Paid,
        _ => PaymentPref::Auto,
    }
}

fn payment_pref_str(p: PaymentPref) -> &'static str {
    match p {
        PaymentPref::Free => "free",
        PaymentPref::Paid => "paid",
        PaymentPref::Auto => "auto",
    }
}

fn resolved_explorer(econ: &p2p_config::EconomicsConfig) -> String {
    if let Some(x) = &econ.active_settings().explorer {
        if !x.trim().is_empty() {
            return x.clone();
        }
    }
    match econ.network {
        TonNetwork::Mainnet => "tonviewer.com".to_string(),
        TonNetwork::Testnet => "testnet.tonviewer.com".to_string(),
    }
}
