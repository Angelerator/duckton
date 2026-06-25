import { invoke } from '@tauri-apps/api/core';
import { openUrl } from '@tauri-apps/plugin-opener';

// --- Types mirror the Rust DTOs in `src-tauri/src/dto.rs` -------------------

export interface NodeStatus {
	running: boolean;
	node_id: string | null;
	listen_addr: string | null;
	uptime_secs: number;
	jobs_served: number;
	local_jobs: number;
	engine_version: string;
	protocol_version: string;
	network: string;
	economics_enabled: boolean;
	settlement: string;
	mainnet_confirmed: boolean;
	default_payment: string;
	wallet_address: string | null;
	memory_bytes: number;
	threads: number;
	max_jobs: number;
	data_classes: string[];
	bootstrap: string[];
	bind_addr: string;
	unit_price: number;
	max_bid: number;
	fee_recipient: string | null;
	stake_vault: string | null;
	global_params: string | null;
	job_escrow: string | null;
	record_anchor: string | null;
	rpc_endpoint: string;
	explorer: string;
}

export interface ConfigView {
	bind_addr: string;
	advertised_addr: string | null;
	memory_bytes: number;
	threads: number;
	max_jobs: number;
	per_job_memory_bytes: number;
	data_classes: string[];
	bootstrap: string[];
	pinning_mode: string;
	security_mode: string;
	mdns: boolean;
	autonat: boolean;
	relay_client: boolean;
	act_as_relay: boolean;
}

export interface EconomicsInput {
	enabled: boolean;
	settlement: string;
	network: string;
	mainnet_confirm: boolean;
	fee_recipient: string | null;
	default_payment: string;
}

export interface WalletInput {
	network: string;
	address: string | null;
	mnemonic: string | null;
	api_key: string | null;
}

export interface ContractsInput {
	network: string;
	global_params: string | null;
	stake_vault: string | null;
	job_escrow: string | null;
	record_anchor: string | null;
}

export interface PricingInput {
	unit_price: number;
	max_bid: number;
}

export interface ActionResult {
	ok: boolean;
	message: string;
	tx: string | null;
}

// --- Command wrappers -------------------------------------------------------

export const getStatus = () => invoke<NodeStatus>('get_status');
export const startNode = () => invoke<NodeStatus>('start_node');
export const stopNode = () => invoke<NodeStatus>('stop_node');
export const getConfig = () => invoke<ConfigView>('get_config');
export const saveConfig = (config: ConfigView) => invoke<void>('save_config', { config });
export const getLogs = () => invoke<string[]>('get_logs');
export const setEconomics = (input: EconomicsInput) => invoke<NodeStatus>('set_economics', { input });
export const setWallet = (input: WalletInput) => invoke<NodeStatus>('set_wallet', { input });
export const setContracts = (input: ContractsInput) => invoke<NodeStatus>('set_contracts', { input });
export const setPricing = (input: PricingInput) => invoke<NodeStatus>('set_pricing', { input });
export const stake = (amount: number) => invoke<ActionResult>('stake', { amount });
export const unstake = (amount: number) => invoke<ActionResult>('unstake', { amount });

/** Open a URL in the user's default browser (tauri-plugin-opener). */
export async function openExternal(url: string) {
	try {
		await openUrl(url);
	} catch (e) {
		console.error('openExternal failed', e);
	}
}
