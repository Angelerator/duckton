//! Serde data-transfer objects exchanged with the Svelte frontend over the Tauri
//! command boundary. Kept deliberately flat + string-keyed so the UI never has to
//! know the internal `GridConfig` shape.

use serde::{Deserialize, Serialize};

/// Live + configured state of the node, surfaced on the dashboard.
#[derive(Serialize, Clone, Debug, Default)]
pub struct NodeStatus {
    pub running: bool,
    pub node_id: Option<String>,
    pub listen_addr: Option<String>,
    pub uptime_secs: u64,
    pub jobs_served: u64,
    pub local_jobs: u64,
    pub engine_version: String,
    pub protocol_version: String,

    // Config snapshot (so the dashboard reflects what a (re)start would use).
    pub network: String,
    pub economics_enabled: bool,
    pub settlement: String,
    pub mainnet_confirmed: bool,
    pub default_payment: String,
    pub wallet_address: Option<String>,
    pub memory_bytes: u64,
    pub threads: u32,
    pub max_jobs: u32,
    pub data_classes: Vec<String>,
    pub bootstrap: Vec<String>,
    pub bind_addr: String,
    pub unit_price: u64,
    pub max_bid: u64,
    pub fee_recipient: Option<String>,
    pub stake_vault: Option<String>,
    pub global_params: Option<String>,
    pub job_escrow: Option<String>,
    pub record_anchor: Option<String>,
    pub rpc_endpoint: String,
    pub explorer: String,
}

/// The host-relevant config the Configuration screen edits. Advanced knobs in
/// `GridConfig` are preserved untouched (we only overwrite these fields).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ConfigView {
    pub bind_addr: String,
    pub advertised_addr: Option<String>,
    pub memory_bytes: u64,
    pub threads: u32,
    pub max_jobs: u32,
    pub per_job_memory_bytes: u64,
    pub data_classes: Vec<String>,
    pub bootstrap: Vec<String>,
    pub pinning_mode: String,
    pub security_mode: String,
    pub mdns: bool,
    pub autonat: bool,
    pub relay_client: bool,
    pub act_as_relay: bool,
}

/// Payments → economics rail + network selection (testnet/mainnet).
#[derive(Deserialize, Clone, Debug)]
pub struct EconomicsInput {
    pub enabled: bool,
    /// `noop` | `mock` | `ton`.
    pub settlement: String,
    /// `testnet` | `mainnet`.
    pub network: String,
    /// Explicit opt-in required before mainnet (real funds) is honored.
    pub mainnet_confirm: bool,
    pub fee_recipient: Option<String>,
    /// `free` | `paid` | `auto`.
    pub default_payment: String,
}

/// Payments → wallet for one network. The raw mnemonic (when present) is written
/// by the backend to a `0600` file and only its path is stored.
#[derive(Deserialize, Clone, Debug)]
pub struct WalletInput {
    pub network: String,
    pub address: Option<String>,
    pub mnemonic: Option<String>,
    pub api_key: Option<String>,
}

/// Payments → on-chain contract addresses for one network.
#[derive(Deserialize, Clone, Debug)]
pub struct ContractsInput {
    pub network: String,
    pub global_params: Option<String>,
    pub stake_vault: Option<String>,
    pub job_escrow: Option<String>,
    pub record_anchor: Option<String>,
}

/// Payments → provider pricing.
#[derive(Deserialize, Clone, Debug)]
pub struct PricingInput {
    pub unit_price: u64,
    pub max_bid: u64,
}

/// Generic result for an action that may broadcast on-chain.
#[derive(Serialize, Clone, Debug)]
pub struct ActionResult {
    pub ok: bool,
    pub message: String,
    pub tx: Option<String>,
}
