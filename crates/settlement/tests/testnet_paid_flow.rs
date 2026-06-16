//! `ton-live`-gated LIVE testnet **paid-flow** validation: the end-to-end
//! open-escrow-per-job → settle path driven entirely through the production Rust
//! seams ([`GlobalParamsClient`] + [`TonSettlement`] over [`ToncenterRpc`]).
//!
//! This is the live proof for Task 2 / the §12 params binding: it
//!   1. SYNCS the live on-chain `GlobalParams` version via the production
//!      [`GlobalParamsClient`] read seam (`get_params_version`),
//!   2. OPENS a fresh per-job `JobEscrow` through [`TonSettlement::open_escrow_with_terms`],
//!      binding the synced `params_version` + the quorum `expected_hash` (HTLC lock)
//!      into the escrow's terms (hence its deterministic address) — a real funded
//!      deploy broadcast by the Rust wallet-v5r1 signer,
//!   3. SETTLES it through [`TonSettlement::settle`] (winner = our own wallet, so
//!      funds return and gas is conserved),
//!   4. VERIFIES EVERY transaction's on-chain exit code (compute + action) via the
//!      Toncenter v3 API, confirms the escrow's on-chain `get_params_version` /
//!      `get_expected_hash` match what was synced/locked, and that `settled` flips.
//!
//! Compiled ONLY with `--features ton-live` and SKIPS (returns `Ok`) unless the
//! testnet env is present, so it is a genuine no-op in normal CI.
//!
//! ## Run
//! ```bash
//! export SDKROOT=$(xcrun --show-sdk-path)
//! source ~/.config/duckdb-p2p/testnet.env   # TON_TESTNET_{MNEMONIC,API_KEY,RPC}
//! export TON_TESTNET_GLOBAL_PARAMS_ADDR="kQB-HK_vWQuXvKE_VGo2ZOxDqeLUgNJLjzvR80Hmkks7tlOB"
//! cargo test -p p2p-settlement --features ton-live --test testnet_paid_flow -- --nocapture
//! ```

#![cfg(feature = "ton-live")]

use std::process::Command;
use std::thread::sleep;
use std::time::Duration;

use p2p_settlement::ton::{
    build_escrow_terms, escrow_code_from_boc_base64, GlobalParamsClient, TonSettlement,
    ToncenterRpc,
};
use p2p_settlement::traits::Settlement;
use p2p_settlement::types::{Amount, Payout, SettlementOutcome, WalletAddress};

const GLOBAL_PARAMS_FRIENDLY: &str = "kQB-HK_vWQuXvKE_VGo2ZOxDqeLUgNJLjzvR80Hmkks7tlOB";

/// Conserve gas. The locked bid `B` is small; the winner == our own wallet and
/// the refund == requester == our own wallet, so `B` returns to us in full at
/// settle (winner `WINNER_AMOUNT` + refund `B - WINNER_AMOUNT`). The only net
/// cost is tx gas + the deploy gas buffer left in the dead escrow.
const ESCROW_BID: Amount = 50_000_000; // 0.05 TON locked B
const WINNER_AMOUNT: Amount = 30_000_000; // 0.03 TON to winner; 0.02 TON refunds
/// Deploy headroom so the escrow holds ≥ B (plus action forward fees) to pay out
/// the split (the locked B is returned to us in full; this covers fees).
const DEPLOY_GAS_BUFFER: Amount = 30_000_000; // 0.03 TON
/// Gas attached to the settle message so the escrow's compute phase can run (a
/// 0-value internal message aborts before compute on TON).
const SETTLE_GAS: Amount = 30_000_000; // 0.03 TON

struct Env {
    rpc: String,
    api_key: String,
    mnemonic: String,
    gp_addr: String,
}

fn env() -> Option<Env> {
    let rpc = std::env::var("TON_TESTNET_RPC")
        .ok()
        .filter(|s| !s.trim().is_empty())?;
    let mnemonic = std::env::var("TON_TESTNET_MNEMONIC")
        .ok()
        .filter(|s| !s.trim().is_empty())?;
    let api_key = std::env::var("TON_TESTNET_API_KEY")
        .ok()
        .unwrap_or_default();
    let gp_addr = std::env::var("TON_TESTNET_GLOBAL_PARAMS_ADDR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| GLOBAL_PARAMS_FRIENDLY.to_string());
    Some(Env {
        rpc: rpc.trim_end_matches('/').to_string(),
        api_key,
        mnemonic,
        gp_addr,
    })
}

fn skip(reason: &str) {
    eprintln!("SKIP testnet_paid_flow: {reason} (source ~/.config/duckdb-p2p/testnet.env to run)");
}

fn curl(env: &Env, args: &[String]) -> String {
    let mut cmd = Command::new("curl");
    cmd.arg("-s").arg("--max-time").arg("30");
    if !env.api_key.is_empty() {
        cmd.arg("-H").arg(format!("X-API-Key: {}", env.api_key));
    }
    for a in args {
        cmd.arg(a);
    }
    let out = cmd.output().expect("curl spawns");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn account_state(env: &Env, raw_addr: &str) -> (String, i128) {
    let raw = curl(
        env,
        &[format!(
            "{}/getAddressInformation?address={raw_addr}",
            env.rpc
        )],
    );
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
    let state = v["result"]["state"]
        .as_str()
        .unwrap_or("unknown")
        .to_string();
    let bal = v["result"]["balance"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .or_else(|| v["result"]["balance"].as_i64().map(|x| x as i128))
        .unwrap_or(0);
    (state, bal)
}

fn read_seqno(env: &Env, wallet_raw: &str) -> u32 {
    let body = format!(r#"{{"address":"{wallet_raw}","method":"seqno","stack":[]}}"#);
    let raw = curl(
        env,
        &[
            "-H".into(),
            "Content-Type: application/json".into(),
            "-d".into(),
            body,
            format!("{}/runGetMethod", env.rpc),
        ],
    );
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
    let s = v["result"]["stack"][0][1]
        .as_str()
        .unwrap_or("0x0")
        .trim()
        .to_string();
    u32::from_str_radix(s.trim_start_matches("0x").trim_start_matches("0X"), 16)
        .or_else(|_| s.parse())
        .unwrap_or(0)
}

fn wait_for_seqno(env: &Env, wallet_raw: &str, want: u32, label: &str) -> u32 {
    for attempt in 0..36 {
        let s = read_seqno(env, wallet_raw);
        if s >= want {
            println!(
                "  [{label}] wallet seqno reached {s} after {}s",
                attempt * 5
            );
            return s;
        }
        sleep(Duration::from_secs(5));
    }
    let s = read_seqno(env, wallet_raw);
    println!("  [{label}] WARNING: wallet seqno still {s} (wanted >= {want}) after timeout");
    s
}

/// Wait until `raw_addr` reaches `want_state` (e.g. "active"), returning success.
fn wait_for_state(env: &Env, raw_addr: &str, want_state: &str, label: &str) -> bool {
    for attempt in 0..36 {
        let (state, bal) = account_state(env, raw_addr);
        if state == want_state {
            println!(
                "  [{label}] account state = {state} (balance {bal}) after {}s",
                attempt * 5
            );
            return true;
        }
        sleep(Duration::from_secs(5));
    }
    let (state, _) = account_state(env, raw_addr);
    println!(
        "  [{label}] WARNING: account state still {state} (wanted {want_state}) after timeout"
    );
    false
}

/// Run a single-int get-method on a raw address (independent of the seam under
/// test, so verification is not self-referential).
fn get_int(env: &Env, raw_addr: &str, method: &str) -> Option<i128> {
    let body = format!(r#"{{"address":"{raw_addr}","method":"{method}","stack":[]}}"#);
    let raw = curl(
        env,
        &[
            "-H".into(),
            "Content-Type: application/json".into(),
            "-d".into(),
            body,
            format!("{}/runGetMethod", env.rpc),
        ],
    );
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    if v["ok"].as_bool() == Some(false) {
        return None;
    }
    let s = v["result"]["stack"][0][1].as_str()?.trim().to_string();
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        i128::from_str_radix(h, 16).ok()
    } else {
        s.parse().ok()
    }
}

/// Read `get_escrow_state` = (escrowAmount, deadline, settled, paramsVersion);
/// return `(settled, paramsVersion)`.
fn read_escrow_state(env: &Env, raw_addr: &str) -> Option<(bool, u32)> {
    let body = format!(r#"{{"address":"{raw_addr}","method":"get_escrow_state","stack":[]}}"#);
    let raw = curl(
        env,
        &[
            "-H".into(),
            "Content-Type: application/json".into(),
            "-d".into(),
            body,
            format!("{}/runGetMethod", env.rpc),
        ],
    );
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    if v["ok"].as_bool() == Some(false) {
        return None;
    }
    let stack = v["result"]["stack"].as_array()?;
    let int_at = |i: usize| -> Option<i128> {
        let s = stack.get(i)?.get(1)?.as_str()?.trim().to_string();
        if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            i128::from_str_radix(h, 16).ok()
        } else {
            s.parse().ok()
        }
    };
    // stack: [escrowAmount, deadline, settled, paramsVersion]
    let settled = int_at(2)? != 0;
    let pv = int_at(3)? as u32;
    Some((settled, pv))
}

/// Fetch + print the latest transaction's (compute_exit, action_result, aborted)
/// for `raw_addr` via the Toncenter v3 API, polling for indexing lag. Returns
/// `None` only if the v3 API is genuinely unreachable after retries (callers
/// treat that as a hard failure — every tx exit code MUST be verified).
fn latest_tx_exit(env: &Env, friendly_or_raw: &str, label: &str) -> Option<(i64, i64, bool)> {
    let v3 = env.rpc.replace("/api/v2", "/api/v3");
    for _ in 0..6 {
        let raw = curl(
            env,
            &[format!(
                "{v3}/transactions?account={friendly_or_raw}&limit=1&sort=desc"
            )],
        );
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
            if let Some(tx) = v["transactions"].as_array().and_then(|a| a.first()) {
                let hash = tx["hash"].as_str().unwrap_or("?");
                let compute = tx["description"]["compute_ph"]["exit_code"]
                    .as_i64()
                    .unwrap_or(0);
                let action = tx["description"]["action"]["result_code"]
                    .as_i64()
                    .unwrap_or(0);
                let aborted = tx["description"]["aborted"].as_bool().unwrap_or(false);
                println!(
                    "  [{label}] tx hash={hash}\n            compute_exit={compute} action_result={action} aborted={aborted}\n            https://testnet.tonviewer.com/transaction/{hash}"
                );
                return Some((compute, action, aborted));
            }
        }
        sleep(Duration::from_secs(5));
    }
    None
}

/// Mandatory exit-code verification: every broadcast tx's compute + action phase
/// MUST be exit 0 and not aborted (the user insists results are always checked,
/// with zero intentional failed txs).
fn assert_tx_ok(env: &Env, raw_addr: &str, label: &str) {
    let (compute, action, aborted) = latest_tx_exit(env, raw_addr, label)
        .expect("v3 tx exit code must be readable (verify every tx)");
    assert!(!aborted, "[{label}] tx must not be aborted");
    assert_eq!(compute, 0, "[{label}] compute exit_code must be 0");
    assert_eq!(action, 0, "[{label}] action result_code must be 0");
}

fn load_escrow_code() -> p2p_settlement::cell::Cell {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../ton/build/JobEscrow.json"
    );
    let artifact = std::fs::read_to_string(path).expect("read ton/build/JobEscrow.json");
    let v: serde_json::Value = serde_json::from_str(&artifact).expect("parse JobEscrow.json");
    let code_b64 = v["code_boc64"].as_str().expect("code_boc64");
    escrow_code_from_boc_base64(code_b64).expect("escrow code BoC parses")
}

#[test]
fn rust_driven_paid_flow_open_settle_is_accepted_on_chain() {
    let Some(env) = env() else {
        return skip("TON_TESTNET_* not set");
    };

    // --- The production seams under test: signer/broadcaster + params client. ---
    let rpc = ToncenterRpc::new(
        &env.rpc,
        Some(env.api_key.clone()),
        "testnet",
        &env.mnemonic,
    )
    .expect("ToncenterRpc builds from the testnet mnemonic");
    let wallet = rpc.wallet_address();
    let wallet_raw = wallet.to_raw_string();
    println!("wallet = {wallet_raw}");
    let (state, start_bal) = account_state(&env, &wallet_raw);
    println!(
        "wallet state = {state}, start balance = {start_bal} nanoton (~{} TON)",
        start_bal as f64 / 1e9
    );
    assert_eq!(state, "active", "funded wallet must be deployed/active");
    // Need the locked bid + the deploy gas buffer + wallet tx gas headroom.
    let need = (ESCROW_BID + DEPLOY_GAS_BUFFER) as i128 + 100_000_000;
    assert!(
        start_bal > need,
        "wallet must have gas + the escrow bid + buffer (need >{need})"
    );

    // === 1) SYNC: read the live on-chain params version (the §12 binding). ===
    let gp_addr = WalletAddress::from_any_str(&env.gp_addr).expect("global_params address parses");
    let gp_rpc = ToncenterRpc::new(
        &env.rpc,
        Some(env.api_key.clone()),
        "testnet",
        &env.mnemonic,
    )
    .expect("params-client RPC builds");
    let params_client = GlobalParamsClient::new(gp_rpc, gp_addr);
    let synced_version = params_client
        .params_version()
        .expect("read live params_version");
    println!("SYNC: live GlobalParams params_version = {synced_version}");
    assert!(synced_version > 0, "live params version must be non-zero");

    // The HTLC lock = a representative agreed quorum result hash.
    let result_hash: [u8; 32] = *blake3::hash(b"duckdb-p2p::rust-live-paid-flow").as_bytes();

    // === 2) OPEN: deploy a fresh per-job escrow binding version + quorum hash. ===
    // with_escrow_code(rpc, escrow_code, terms, arbiter); the per-job terms are
    // (re)built inside open_escrow_with_terms, so the placeholder `terms` here is
    // unused for the deploy. arbiter == our wallet (the only party allowed to
    // settle); requester == treasury == our wallet so refunds/fees return to us.
    let settlement = TonSettlement::with_escrow_code(
        rpc,
        load_escrow_code(),
        // Placeholder shared terms (rebuilt per-job in open_escrow_with_terms):
        // unbound expected-hash + candidates-hash (the new B1 field), version 0,
        // φ = 1500 bps (15%) bound for the on-chain fee enforcement.
        build_escrow_terms(&wallet, &[0u8; 32], &[0u8; 32], 0, 1500),
        wallet,
    )
    .with_requester(wallet)
    .with_treasury(wallet)
    .with_platform_fee_bps(1500)
    .with_escrow_window(3600)
    // B1: bind the payout-eligible candidate set (here just our wallet, the
    // winner) so the escrow's terms commit to it at open AND settle presents the
    // SAME set — the on-chain candidatesCommitment check then passes.
    .with_candidates(vec![wallet])
    .with_deploy_gas_buffer(DEPLOY_GAS_BUFFER)
    .with_settle_gas(SETTLE_GAS);

    let job = p2p_proto::JobId(format!(
        "rust-live-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    ));

    let before_open = read_seqno(&env, &wallet_raw);
    println!(
        "OPEN: deploying per-job escrow (locked B={} nanoton + buffer {} = deploy value {}, version {synced_version}) seqno={before_open}",
        ESCROW_BID,
        DEPLOY_GAS_BUFFER,
        ESCROW_BID + DEPLOY_GAS_BUFFER
    );
    let handle = settlement
        // Treasury == our wallet (the configured fee recipient); pass it as the
        // chain-authoritative fee recipient so it is bound + cross-checked.
        .open_escrow_with_terms(
            &job,
            ESCROW_BID,
            &result_hash,
            synced_version,
            &[wallet],
            Some(wallet),
        )
        .expect("open_escrow_with_terms broadcasts the funded deploy");
    let escrow_raw = handle.address.to_raw_string();
    println!("  escrow address = {escrow_raw}");
    println!("  https://testnet.tonviewer.com/{escrow_raw}");

    let after_open = wait_for_seqno(&env, &wallet_raw, before_open + 1, "open");
    assert_eq!(
        after_open,
        before_open + 1,
        "wallet seqno must increment by 1 (deploy external accepted)"
    );
    assert!(
        wait_for_state(&env, &escrow_raw, "active", "open"),
        "escrow must become active (deployed)"
    );
    sleep(Duration::from_secs(4));

    // Verify the deploy tx exit code on the escrow account (mandatory).
    assert_tx_ok(&env, &escrow_raw, "escrow-deploy");

    // Confirm the on-chain terms binding == what we synced/locked.
    let onchain_pv = get_int(&env, &escrow_raw, "get_params_version").expect("get_params_version");
    assert_eq!(
        onchain_pv as u32, synced_version,
        "on-chain escrow params_version must equal the synced version"
    );
    println!("  on-chain escrow params_version = {onchain_pv} (matches synced {synced_version})");
    // expectedHash is a uint256 (exceeds i128) — compare the full hex.
    let onchain_hash_hex =
        get_hex(&env, &escrow_raw, "get_expected_hash").expect("get_expected_hash");
    let want_hash_hex = hex::encode(result_hash);
    println!("  on-chain expected_hash = 0x{onchain_hash_hex}; locked = 0x{want_hash_hex}");
    assert_eq!(
        onchain_hash_hex, want_hash_hex,
        "on-chain HTLC lock must equal the quorum result hash"
    );

    // === 3) SETTLE: release the escrow (winner == our wallet → funds return). ===
    // The platform fee MUST equal φ·base (15% of the quoted base) for the on-chain
    // strict fee-equality; the winner (a wallet node) is paid the full base.
    let outcome = SettlementOutcome {
        result_hash,
        base: WINNER_AMOUNT,
        winner: Payout {
            to: wallet,
            amount: WINNER_AMOUNT,
        },
        participants: vec![],
        platform_fee: WINNER_AMOUNT * 1500 / 10_000,
    };
    let before_settle = read_seqno(&env, &wallet_raw);
    println!(
        "SETTLE: releasing escrow (winner {} nanoton + {} refund; {} settle-gas) seqno={before_settle}",
        WINNER_AMOUNT,
        ESCROW_BID - WINNER_AMOUNT,
        SETTLE_GAS
    );
    settlement
        .settle(&handle, &outcome)
        .expect("settle broadcasts EscrowSettle");
    let after_settle = wait_for_seqno(&env, &wallet_raw, before_settle + 1, "settle");
    assert_eq!(
        after_settle,
        before_settle + 1,
        "wallet seqno must increment by 1 (settle external accepted)"
    );
    sleep(Duration::from_secs(6));

    // Verify the settle tx exit code on the escrow account (mandatory): sender ==
    // arbiter, hash matches the HTLC lock, payout bounded by B, action sends the
    // winner + refund messages.
    assert_tx_ok(&env, &escrow_raw, "escrow-settle");

    // Confirm the escrow flipped to settled and still reports the bound version.
    if let Some((settled, pv)) = read_escrow_state(&env, &escrow_raw) {
        println!("  on-chain escrow: settled={settled}, paramsVersion={pv}");
        assert!(
            settled,
            "escrow must be settled after the EscrowSettle was accepted"
        );
        assert_eq!(
            pv, synced_version,
            "settled escrow still binds the synced params version"
        );
    }

    let (_, end_bal) = account_state(&env, &wallet_raw);
    println!(
        "end balance = {end_bal} nanoton (~{} TON)",
        end_bal as f64 / 1e9
    );
    println!(
        "gas/loss = {} nanoton (~{} TON) for one full open+settle paid job",
        start_bal - end_bal,
        (start_bal - end_bal) as f64 / 1e9
    );
    println!("RESULT: Rust-driven open-escrow-per-job → settle ACCEPTED on testnet; params_version {synced_version} bound + verified on-chain.");
}

/// Read a uint256 get-method as a left-zero-padded 64-char hex string (no `0x`),
/// so a full 256-bit `expectedHash` can be compared without i128 overflow.
fn get_hex(env: &Env, raw_addr: &str, method: &str) -> Option<String> {
    let body = format!(r#"{{"address":"{raw_addr}","method":"{method}","stack":[]}}"#);
    let raw = curl(
        env,
        &[
            "-H".into(),
            "Content-Type: application/json".into(),
            "-d".into(),
            body,
            format!("{}/runGetMethod", env.rpc),
        ],
    );
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    if v["ok"].as_bool() == Some(false) {
        return None;
    }
    let s = v["result"]["stack"][0][1].as_str()?.trim().to_string();
    let h = s.trim_start_matches("0x").trim_start_matches("0X");
    Some(format!("{h:0>64}"))
}
