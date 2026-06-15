//! `ton-live`-gated LIVE testnet **broadcast** validation for the hand-rolled
//! Rust wallet-v5r1 signer ([`p2p_settlement::wallet`]) + [`ToncenterRpc`].
//!
//! Unlike `testnet_live.rs` (read-only), this harness proves the missing half:
//! that a wallet-v5r1 external message **built and Ed25519-signed entirely in
//! Rust** is ACCEPTED by a live contract on TON testnet. It drives the production
//! [`ToncenterRpc`] (sign → BoC → `sendBoc`) against the deployed `GlobalParams`
//! contract (admin == our wallet):
//!
//!   1. read the wallet seqno + current `get_params` (decoding the on-chain
//!      `EcoParams` cell + `feeRecipient` back into typed values),
//!   2. broadcast an admin `update_params` that toggles `platformFeeBps`
//!      (250 ⇆ 300) — a cheap, reversible, admin-gated op,
//!   3. confirm the wallet seqno incremented (⇒ the v5r1 external was accepted)
//!      AND `get_params` shows the new value (⇒ the internal body was accepted,
//!      validated, and `setData` ran — i.e. compute/action succeeded, no bounce),
//!   4. set `platformFeeBps` back to its original value.
//!
//! Compiled ONLY with `--features ton-live`, and SKIPS (returns `Ok`) unless the
//! testnet env is present, so it is a genuine no-op in normal CI.
//!
//! ## Run
//! ```bash
//! export SDKROOT=$(xcrun --show-sdk-path)
//! export CXXFLAGS="-isystem $(xcrun --show-sdk-path)/usr/include/c++/v1"
//! source ~/.config/duckdb-p2p/testnet.env   # TON_TESTNET_{MNEMONIC,API_KEY,RPC}
//! cargo test -p p2p-settlement --features ton-live --test testnet_broadcast -- --nocapture
//! ```

#![cfg(feature = "ton-live")]

use std::process::Command;
use std::thread::sleep;
use std::time::Duration;

use p2p_settlement::cell::Cell;
use p2p_settlement::ton::{
    build_anchor_submit, build_update_params, GlobalParams, TonRpc, ToncenterRpc,
};
use p2p_settlement::types::WalletAddress;

const WALLET_FRIENDLY: &str = "kQCP7UqEfNwpaaNGDP3MihPPBb-Yd5ZYc0EU-VbXcmjpg422";
const GLOBAL_PARAMS_FRIENDLY: &str = "kQB-HK_vWQuXvKE_VGo2ZOxDqeLUgNJLjzvR80Hmkks7tlOB";
const ANCHOR_FRIENDLY: &str = "kQDccDppcJBwt1mFprymqXE8fDk-w9udKlaeA81bgysW6DCc";

/// ~0.05 TON of gas attached to each admin op (returned excess bounces back).
const GAS_NANOTON: u128 = 50_000_000;

struct Env {
    rpc: String,
    api_key: String,
    mnemonic: String,
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
    Some(Env {
        rpc: rpc.trim_end_matches('/').to_string(),
        api_key,
        mnemonic,
    })
}

fn skip(reason: &str) {
    eprintln!("SKIP testnet_broadcast: {reason} (source ~/.config/duckdb-p2p/testnet.env to run)");
}

// ---------------------------------------------------------------------------
// Minimal curl helpers (reads + tx inspection) — independent of the signer path
// under test, so verification can't be fooled by a bug in the broadcaster.
// ---------------------------------------------------------------------------

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

fn run_get_method(env: &Env, addr: &str, method: &str) -> serde_json::Value {
    let body = format!(r#"{{"address":"{addr}","method":"{method}","stack":[]}}"#);
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
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("bad runGetMethod JSON: {e}\n{raw}"))
}

fn balance(env: &Env, friendly: &str) -> i128 {
    let raw = curl(
        env,
        &[format!(
            "{}/getAddressInformation?address={friendly}",
            env.rpc
        )],
    );
    let v: serde_json::Value = serde_json::from_str(&raw).expect("balance JSON");
    v["result"]["balance"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .or_else(|| v["result"]["balance"].as_i64().map(|x| x as i128))
        .unwrap_or(0)
}

/// Standard/url-safe base64 decode (padding optional) — for the BoC/cell-data
/// blobs Toncenter returns in get-method stack entries.
fn b64_decode(s: &str) -> Vec<u8> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' | b'-' => Some(62),
            b'/' | b'_' => Some(63),
            _ => None,
        }
    }
    let s = s.trim().trim_end_matches('=');
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut buf, mut bits) = (0u32, 0u32);
    for &c in s.as_bytes() {
        let Some(v) = val(c) else { continue };
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    out
}

/// Turn a get-method stack entry of `@type` cell/slice into a [`Cell`]. The
/// entries we read here are ref-less (an address slice, the flat EcoParams cell),
/// so the decoded `object.data` bits reconstruct the cell exactly.
fn stack_cell(entry: &serde_json::Value) -> Cell {
    use p2p_settlement::cell::CellBuilder;
    let obj = &entry[1]["object"]["data"];
    let bits = obj["len"].as_u64().expect("cell bit len") as usize;
    let data = b64_decode(obj["b64"].as_str().expect("cell data b64"));
    CellBuilder::new().store_bits(&data, bits).build()
}

/// Parse the on-chain `EcoParams` cell back into a typed [`GlobalParams`], in the
/// exact field order of `ton/contracts/global_params_types.tolk::EcoParams`.
fn parse_eco_params(cell: &Cell) -> GlobalParams {
    let mut p = cell.parser();
    let u16f = |p: &mut p2p_settlement::cell::CellParser| p.load_uint(16).unwrap() as u16;
    let mut g = GlobalParams {
        platform_fee_bps: 0,
        surcharge_bps: 0,
        participation_commission_bps: 0,
        slash_wrong_bps: 0,
        slash_cheat_bps: 0,
        slash_downtime_bps: 0,
        slash_equivocation_bps: 0,
        split_challenger_bps: 0,
        split_redundancy_bps: 0,
        split_burn_bps: 0,
        split_treasury_bps: 0,
        min_stake: 0,
        min_stake_internal: 0,
        min_stake_sensitive: 0,
        stake_cap: 0,
        unbonding_secs: 0,
        challenge_window_secs: 0,
        n_public: 0,
        n_default: 0,
        n_max: 0,
        quorum: 0,
        checksum_min: 0,
        w_quality_bps: 0,
        w_stake_bps: 0,
        w_price_bps: 0,
        slash_failed_commitment_bps: 0,
        attempt_deadline_ms: 0,
        progress_interval_ms: 0,
        progress_stall_mult: 0,
    };
    g.platform_fee_bps = u16f(&mut p);
    g.surcharge_bps = u16f(&mut p);
    g.participation_commission_bps = u16f(&mut p);
    g.slash_wrong_bps = u16f(&mut p);
    g.slash_cheat_bps = u16f(&mut p);
    g.slash_downtime_bps = u16f(&mut p);
    g.slash_equivocation_bps = u16f(&mut p);
    g.split_challenger_bps = u16f(&mut p);
    g.split_redundancy_bps = u16f(&mut p);
    g.split_burn_bps = u16f(&mut p);
    g.split_treasury_bps = u16f(&mut p);
    g.min_stake = p.load_coins().unwrap();
    g.min_stake_internal = p.load_coins().unwrap();
    g.min_stake_sensitive = p.load_coins().unwrap();
    g.stake_cap = p.load_coins().unwrap();
    g.unbonding_secs = p.load_uint(32).unwrap() as u32;
    g.challenge_window_secs = p.load_uint(32).unwrap() as u32;
    g.n_public = p.load_uint(8).unwrap() as u8;
    g.n_default = p.load_uint(8).unwrap() as u8;
    g.n_max = p.load_uint(8).unwrap() as u8;
    g.quorum = p.load_uint(8).unwrap() as u8;
    g.checksum_min = p.load_uint(8).unwrap() as u8;
    g.w_quality_bps = u16f(&mut p);
    g.w_stake_bps = u16f(&mut p);
    g.w_price_bps = u16f(&mut p);
    g.slash_failed_commitment_bps = u16f(&mut p);
    g.attempt_deadline_ms = p.load_uint(32).unwrap() as u32;
    g.progress_interval_ms = p.load_uint(32).unwrap() as u32;
    g.progress_stall_mult = p.load_uint(8).unwrap() as u8;
    g
}

/// Read `(feeRecipient, params)` from `GlobalParams.get_params` (stack: admin,
/// feeRecipient, params).
fn read_params(env: &Env) -> (WalletAddress, GlobalParams) {
    let v = run_get_method(env, GLOBAL_PARAMS_FRIENDLY, "get_params");
    assert_eq!(v["ok"].as_bool(), Some(true), "get_params ok: {v}");
    let stack = v["result"]["stack"].as_array().expect("stack");
    assert_eq!(
        stack.len(),
        3,
        "get_params returns admin, feeRecipient, params"
    );
    let fee_recipient = stack_cell(&stack[1])
        .parser()
        .load_address()
        .expect("feeRecipient addr");
    let params = parse_eco_params(&stack_cell(&stack[2]));
    (fee_recipient, params)
}

/// Parse a Toncenter hex num (`0x...`) into a left-zero-padded 32-byte big-endian
/// array (an on-chain `uint256` root).
fn hex_to_32(s: &str) -> [u8; 32] {
    let h = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    let h = if h.len() % 2 == 1 {
        format!("0{h}")
    } else {
        h.to_string()
    };
    let raw = (0..h.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap())
        .collect::<Vec<_>>();
    let mut out = [0u8; 32];
    out[32 - raw.len()..].copy_from_slice(&raw);
    out
}

/// Read `(currentEpoch, lastRoot)` from `RecordAnchor.get_anchor_state` (stack:
/// currentEpoch, lastRoot, nextDisputeId).
fn read_anchor_state(env: &Env) -> (u32, [u8; 32]) {
    let v = run_get_method(env, ANCHOR_FRIENDLY, "get_anchor_state");
    assert_eq!(v["ok"].as_bool(), Some(true), "get_anchor_state ok: {v}");
    let stack = v["result"]["stack"].as_array().expect("stack");
    let epoch_s = stack[0][1].as_str().unwrap_or("0x0").trim().to_string();
    let epoch = u32::from_str_radix(epoch_s.trim_start_matches("0x"), 16)
        .or_else(|_| epoch_s.parse())
        .unwrap_or(0);
    let last_root = hex_to_32(stack[1][1].as_str().unwrap_or("0x0"));
    (epoch, last_root)
}

fn read_seqno(env: &Env) -> u32 {
    let v = run_get_method(env, WALLET_FRIENDLY, "seqno");
    let s = v["result"]["stack"][0][1].as_str().unwrap_or("0x0");
    let s = s.trim();
    u32::from_str_radix(s.trim_start_matches("0x").trim_start_matches("0X"), 16)
        .or_else(|_| s.parse())
        .unwrap_or(0)
}

/// Poll the wallet seqno until it reaches `want` (or time out). Returns the last
/// observed seqno.
fn wait_for_seqno(env: &Env, want: u32, label: &str) -> u32 {
    for attempt in 0..30 {
        let s = read_seqno(env);
        if s >= want {
            println!("  [{label}] seqno reached {s} after {}s", attempt * 5);
            return s;
        }
        sleep(Duration::from_secs(5));
    }
    let s = read_seqno(env);
    println!("  [{label}] WARNING: seqno still {s} (wanted >= {want}) after timeout");
    s
}

/// Best-effort: print the latest transactions (+ exit codes) for an address via
/// the Toncenter v3 API, for the human report. Never fails the test.
fn print_recent_txs(env: &Env, friendly: &str, label: &str) {
    let v3 = env.rpc.replace("/api/v2", "/api/v3");
    let raw = curl(
        env,
        &[format!(
            "{v3}/transactions?account={friendly}&limit=2&sort=desc"
        )],
    );
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
        println!("  [{label}] (v3 tx fetch unavailable)");
        return;
    };
    let Some(txs) = v["transactions"].as_array() else {
        println!("  [{label}] (no v3 transactions array)");
        return;
    };
    for tx in txs {
        let hash = tx["hash"].as_str().unwrap_or("?");
        let lt = tx["lt"].as_str().unwrap_or("?");
        let compute = &tx["description"]["compute_ph"]["exit_code"];
        let action = &tx["description"]["action"]["result_code"];
        let bounce = &tx["description"]["aborted"];
        println!(
            "  [{label}] tx lt={lt} hash={hash}\n            compute_exit={compute} action_result={action} aborted={bounce}\n            https://testnet.tonviewer.com/transaction/{hash}"
        );
    }
}

/// Broadcast an `update_params` that sets `platform_fee_bps = new_fee`, then wait
/// for the wallet seqno to advance and confirm the on-chain value changed.
fn set_platform_fee(env: &Env, rpc: &ToncenterRpc, gp_addr: &WalletAddress, new_fee: u16) {
    let (fee_recipient, mut params) = read_params(env);
    let before_seqno = read_seqno(env);
    println!(
        "→ update_params: platformFeeBps {} -> {new_fee} (seqno {before_seqno})",
        params.platform_fee_bps
    );
    params.platform_fee_bps = new_fee;
    params
        .validate()
        .expect("toggled params still satisfy the §12 bounds");

    let body = build_update_params(0xC0FFEE, &fee_recipient, &params);
    let res = rpc
        .send_internal(gp_addr, GAS_NANOTON, &body)
        .expect("Rust-signed wallet-v5r1 BoC must be ACCEPTED by sendBoc");
    println!("  sendBoc accepted: {res}");

    let after_seqno = wait_for_seqno(env, before_seqno + 1, "wallet");
    assert_eq!(
        after_seqno,
        before_seqno + 1,
        "wallet seqno must increment by exactly 1 (proves the v5r1 external was accepted)"
    );

    // Settle, then confirm the destination contract actually applied the update.
    sleep(Duration::from_secs(4));
    let (_, after) = read_params(env);
    println!("  on-chain platformFeeBps now = {}", after.platform_fee_bps);
    assert_eq!(
        after.platform_fee_bps, new_fee,
        "GlobalParams state must reflect the update (proves the internal body was accepted, validated, and setData ran — no bounce/throw)"
    );
    print_recent_txs(env, WALLET_FRIENDLY, "wallet");
    print_recent_txs(env, GLOBAL_PARAMS_FRIENDLY, "global_params");
}

#[test]
fn rust_signed_update_params_is_accepted_on_chain() {
    let Some(env) = env() else {
        return skip("TON_TESTNET_* not set");
    };

    // The production signer/broadcaster under test.
    let rpc = ToncenterRpc::new(
        &env.rpc,
        Some(env.api_key.clone()),
        "testnet",
        &env.mnemonic,
    )
    .expect("ToncenterRpc builds from the testnet mnemonic");

    // 1) The Rust-derived wallet address must match the funded testnet wallet.
    let wallet = rpc.wallet_address();
    let expected = WalletAddress::from_base64_str(WALLET_FRIENDLY).unwrap();
    assert_eq!(
        wallet, expected,
        "Rust-derived v5r1 address must equal the funded wallet"
    );
    println!("wallet = {} ({WALLET_FRIENDLY})", wallet.to_raw_string());

    let gp_addr = WalletAddress::from_base64_str(GLOBAL_PARAMS_FRIENDLY).unwrap();

    let start_balance = balance(&env, WALLET_FRIENDLY);
    println!(
        "start balance = {start_balance} nanoton (~{} TON)",
        start_balance as f64 / 1e9
    );

    // Read the current value so the toggle (and revert) are exact + reversible.
    let (_, original) = read_params(&env);
    let original_fee = original.platform_fee_bps;
    let toggled_fee = if original_fee == 300 { 250 } else { 300 };
    println!("original platformFeeBps = {original_fee}; toggling to {toggled_fee}");

    // 2) Toggle, prove acceptance + state change.
    set_platform_fee(&env, &rpc, &gp_addr, toggled_fee);

    // 3) Revert to the original value (leaves the contract as we found it).
    set_platform_fee(&env, &rpc, &gp_addr, original_fee);

    let end_balance = balance(&env, WALLET_FRIENDLY);
    println!(
        "end balance = {end_balance} nanoton (~{} TON)",
        end_balance as f64 / 1e9
    );
    println!(
        "spent = {} nanoton (~{} TON) across 2 admin ops",
        start_balance - end_balance,
        (start_balance - end_balance) as f64 / 1e9
    );

    let (_, restored) = read_params(&env);
    assert_eq!(
        restored.platform_fee_bps, original_fee,
        "platformFeeBps restored to original"
    );
    println!("RESULT: Rust-signed wallet-v5r1 BoC ACCEPTED on-chain; state toggled and restored.");
}

/// A SECOND body shape exercised through the same Rust signer: a permissionless
/// `RecordAnchor` keeper `AnchorSubmitRoot` (opcode + queryId + epoch + TWO
/// 256-bit roots + coins). It chains a fresh epoch root to the stored one and
/// advances `currentEpoch` (monotonic keeper behavior — no funds leave the
/// contract, gas only). Proves the v5r1 signer also carries the heavier anchor
/// body verbatim to a different live contract.
#[test]
fn rust_signed_anchor_submit_is_accepted_on_chain() {
    let Some(env) = env() else {
        return skip("TON_TESTNET_* not set");
    };
    let rpc = ToncenterRpc::new(
        &env.rpc,
        Some(env.api_key.clone()),
        "testnet",
        &env.mnemonic,
    )
    .expect("ToncenterRpc builds");
    let anchor = WalletAddress::from_base64_str(ANCHOR_FRIENDLY).unwrap();

    let (epoch, prev_root) = read_anchor_state(&env);
    let next_epoch = epoch + 1;
    // A fresh, recognizable test root chained onto the stored one.
    let new_root: [u8; 32] =
        *blake3::hash(format!("rust-v5r1-anchor-epoch-{next_epoch}").as_bytes()).as_bytes();
    // stakeWeight is off-chain metadata (NOT funds); clear the 100-TON threshold.
    let stake_weight: u128 = 100_000_000_000;

    let before_seqno = read_seqno(&env);
    println!(
        "→ anchor submit: epoch {epoch} -> {next_epoch}, prevRoot={} (seqno {before_seqno})",
        hex::encode(prev_root)
    );

    let body = build_anchor_submit(0xA9C40, next_epoch, &new_root, &prev_root, stake_weight);
    let res = rpc
        .send_internal(&anchor, GAS_NANOTON, &body)
        .expect("Rust-signed wallet-v5r1 anchor BoC must be ACCEPTED by sendBoc");
    println!("  sendBoc accepted: {res}");

    let after_seqno = wait_for_seqno(&env, before_seqno + 1, "wallet");
    assert_eq!(
        after_seqno,
        before_seqno + 1,
        "wallet seqno must increment by 1"
    );

    sleep(Duration::from_secs(4));
    let (epoch_after, last_after) = read_anchor_state(&env);
    println!(
        "  on-chain currentEpoch now = {epoch_after}, lastRoot = {}",
        hex::encode(last_after)
    );
    assert_eq!(
        epoch_after, next_epoch,
        "currentEpoch must advance to next_epoch (anchor body accepted)"
    );
    assert_eq!(
        last_after, new_root,
        "lastRoot must equal the submitted root"
    );
    print_recent_txs(&env, ANCHOR_FRIENDLY, "anchor");
    println!(
        "RESULT: Rust-signed AnchorSubmitRoot ACCEPTED on-chain; epoch advanced + root chained."
    );
}
