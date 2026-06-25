//! Thin Tauri command wrappers around [`AppState`]. All errors are surfaced to
//! the UI as strings.

use tauri::State;

use p2p_config::TonNetwork;

use crate::config_store;
use crate::dto::{
    ActionResult, ConfigView, ContractsInput, EconomicsInput, NodeStatus, PricingInput, WalletInput,
};
use crate::node_manager::{parse_network, parse_payment, parse_settlement, AppState};

type CmdResult<T> = Result<T, String>;

#[tauri::command]
pub async fn get_status(state: State<'_, AppState>) -> CmdResult<NodeStatus> {
    state.status().await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn start_node(state: State<'_, AppState>) -> CmdResult<NodeStatus> {
    state.start().await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn stop_node(state: State<'_, AppState>) -> CmdResult<NodeStatus> {
    state.stop().await;
    state.status().await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_config(state: State<'_, AppState>) -> CmdResult<ConfigView> {
    Ok(state.config_view().await)
}

#[tauri::command]
pub async fn save_config(state: State<'_, AppState>, config: ConfigView) -> CmdResult<()> {
    state.apply_config(config).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_logs(state: State<'_, AppState>) -> CmdResult<Vec<String>> {
    let buf = state.logs.lock().map_err(|_| "log buffer poisoned")?;
    Ok(buf.iter().cloned().collect())
}

#[tauri::command]
pub async fn set_economics(
    state: State<'_, AppState>,
    input: EconomicsInput,
) -> CmdResult<NodeStatus> {
    {
        let mut cfg = state.config.lock().await;
        let e = &mut cfg.economics;
        e.enabled = input.enabled;
        e.settlement = parse_settlement(&input.settlement);
        e.network = parse_network(&input.network);
        e.mainnet_confirmed = input.mainnet_confirm;
        e.default_payment = parse_payment(&input.default_payment);
        e.fee_recipient = input
            .fee_recipient
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
    }
    state.persist().await.map_err(|e| e.to_string())?;
    state.status().await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn set_wallet(state: State<'_, AppState>, input: WalletInput) -> CmdResult<NodeStatus> {
    let net = parse_network(&input.network);
    let net_name = net_name(net);

    // Persist secrets to 0600 files BEFORE taking the config lock (no fs under lock).
    let mnemonic_path = match input.mnemonic.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        Some(m) => Some(
            config_store::write_secret(&state.paths, &format!("{net_name}.mnemonic"), m)
                .map_err(|e| e.to_string())?,
        ),
        None => None,
    };
    let api_path = match input.api_key.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        Some(k) => Some(
            config_store::write_secret(&state.paths, &format!("{net_name}.api_key"), k)
                .map_err(|e| e.to_string())?,
        ),
        None => None,
    };

    {
        let mut cfg = state.config.lock().await;
        let settings = match net {
            TonNetwork::Mainnet => &mut cfg.economics.mainnet,
            TonNetwork::Testnet => &mut cfg.economics.testnet,
        };
        if let Some(a) = input.address.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
            settings.wallet.address = Some(a);
        }
        if let Some(p) = mnemonic_path {
            settings.wallet.mnemonic_file = Some(p.to_string_lossy().into_owned());
        }
        if let Some(p) = api_path {
            settings.api_key_file = Some(p.to_string_lossy().into_owned());
        }
    }
    state.persist().await.map_err(|e| e.to_string())?;
    state.status().await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn set_contracts(
    state: State<'_, AppState>,
    input: ContractsInput,
) -> CmdResult<NodeStatus> {
    let net = parse_network(&input.network);
    {
        let mut cfg = state.config.lock().await;
        let settings = match net {
            TonNetwork::Mainnet => &mut cfg.economics.mainnet,
            TonNetwork::Testnet => &mut cfg.economics.testnet,
        };
        apply_opt(&mut settings.contracts.global_params, input.global_params);
        apply_opt(&mut settings.contracts.stake_vault, input.stake_vault);
        apply_opt(&mut settings.contracts.job_escrow, input.job_escrow);
        apply_opt(&mut settings.contracts.record_anchor, input.record_anchor);
    }
    state.persist().await.map_err(|e| e.to_string())?;
    state.status().await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn set_pricing(state: State<'_, AppState>, input: PricingInput) -> CmdResult<NodeStatus> {
    {
        let mut cfg = state.config.lock().await;
        cfg.economics.pricing.unit_price = input.unit_price;
        cfg.economics.pricing.max_bid = input.max_bid;
    }
    state.persist().await.map_err(|e| e.to_string())?;
    state.status().await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn stake(state: State<'_, AppState>, amount: u64) -> CmdResult<ActionResult> {
    match state.stake_action("stake", amount).await {
        Ok(tx) => Ok(ActionResult {
            ok: true,
            message: format!("StakeDeposit broadcast — toncenter accepted {amount} TON"),
            tx: Some(tx),
        }),
        Err(e) => Ok(ActionResult { ok: false, message: e.to_string(), tx: None }),
    }
}

#[tauri::command]
pub async fn unstake(state: State<'_, AppState>, amount: u64) -> CmdResult<ActionResult> {
    match state.stake_action("unstake", amount).await {
        Ok(tx) => Ok(ActionResult {
            ok: true,
            message: format!("StakeRequestUnbond broadcast — {amount} TON entering the cooldown"),
            tx: Some(tx),
        }),
        Err(e) => Ok(ActionResult { ok: false, message: e.to_string(), tx: None }),
    }
}

fn net_name(net: TonNetwork) -> &'static str {
    match net {
        TonNetwork::Mainnet => "mainnet",
        TonNetwork::Testnet => "testnet",
    }
}

/// Overwrite `dst` with a trimmed non-empty value; an empty/whitespace input
/// clears the field.
fn apply_opt(dst: &mut Option<String>, input: Option<String>) {
    if let Some(v) = input {
        let v = v.trim().to_string();
        *dst = if v.is_empty() { None } else { Some(v) };
    }
}
