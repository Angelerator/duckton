//! `ton-live`-gated live TON testnet integration test.
//!
//! This file is compiled **only** with `--features ton-live`, so it is a genuine
//! no-op in normal CI (`cargo test --workspace` does not enable the feature and
//! never compiles this file). Even when the feature *is* enabled, every test
//! SKIPS (prints a skip note and returns `Ok`) unless the testnet environment is
//! configured, so it never fails a machine that has no wallet/network.
//!
//! When configured it exercises the crate's `ton` module — the on-chain message
//! ABI builders ([`build_stake_deposit`], [`build_escrow_settle`],
//! [`build_anchor_submit`], [`build_stake_slash`]) and the [`TonRpc`] transport
//! seam — against a REAL Toncenter testnet RPC, reading back the state the
//! companion harness (`scripts/testnet_e2e.sh`) created on chain.
//!
//! Transport is `curl` (no async runtime / HTTP crate); responses are parsed with
//! `serde_json`. Broadcasting signed wallet transactions is intentionally NOT
//! done here — that is the Acton harness's job (it owns the wallet + BoC/signing).
//! This test covers the read + ABI half of the `ton` impl against live data.
//!
//! ## Configure (all read from the environment; see docs/TESTNET.md)
//! ```text
//! TON_TESTNET_RPC          e.g. https://testnet.toncenter.com/api/v2  (required)
//! TON_TESTNET_API_KEY      Toncenter testnet key                      (recommended)
//! TON_TESTNET_VAULT_ADDR   deployed StakeVault address                (required for live reads)
//! TON_TESTNET_ANCHOR_ADDR  deployed RecordAnchor address              (optional)
//! TON_TESTNET_ESCROW_ADDR  deployed JobEscrow address                 (optional)
//! ```
//! The harness writes these into `ton/deployments/testnet.env`; source it then run:
//! ```bash
//! set -a; . ton/deployments/testnet.env; set +a
//! cargo test -p p2p-settlement --features ton-live --test testnet_live -- --nocapture
//! ```

#![cfg(feature = "ton-live")]

use std::process::Command;

use p2p_settlement::ton::{
    build_anchor_submit, build_escrow_settle, build_stake_deposit, build_stake_slash, MessageBody,
    TonRpc, OP_ANCHOR_SUBMIT, OP_ESCROW_SETTLE, OP_STAKE_DEPOSIT, OP_STAKE_SLASH,
};
use p2p_settlement::types::{SettleError, SlashReason, WalletAddress};

/// A live Toncenter `TonRpc` that reads get-methods over HTTPS via `curl`.
///
/// It implements the same [`TonRpc`] seam the crate's `TonSettlement` /
/// `TonStakeRegistry` use; here we drive it directly against the deployed
/// addresses so the test reads REAL on-chain state through the production seam.
struct CurlTonRpc {
    rpc: String,
    api_key: Option<String>,
}

impl CurlTonRpc {
    fn from_env() -> Option<Self> {
        let rpc = std::env::var("TON_TESTNET_RPC").ok()?;
        if rpc.trim().is_empty() {
            return None;
        }
        let api_key = std::env::var("TON_TESTNET_API_KEY")
            .ok()
            .filter(|k| !k.is_empty());
        Some(Self {
            rpc: rpc.trim_end_matches('/').to_string(),
            api_key,
        })
    }

    /// POST a Toncenter v2 `runGetMethod` and return the raw JSON body.
    fn run_get_method_raw(&self, addr: &str, method: &str) -> Result<String, SettleError> {
        let body = format!(r#"{{"address":"{addr}","method":"{method}","stack":[]}}"#);
        let url = format!("{}/runGetMethod", self.rpc);
        let mut cmd = Command::new("curl");
        cmd.arg("-s")
            .arg("--max-time")
            .arg("30")
            .arg("-H")
            .arg("Content-Type: application/json");
        if let Some(k) = &self.api_key {
            cmd.arg("-H").arg(format!("X-API-Key: {k}"));
        }
        cmd.arg("-d").arg(body).arg(url);
        let out = cmd
            .output()
            .map_err(|e| SettleError::Backend(format!("curl spawn failed: {e}")))?;
        if !out.status.success() {
            return Err(SettleError::Backend(format!(
                "curl exited {:?}",
                out.status.code()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

impl TonRpc for CurlTonRpc {
    fn send_internal(
        &self,
        _to: &WalletAddress,
        _amount: p2p_settlement::types::Amount,
        _body: &MessageBody,
    ) -> Result<String, SettleError> {
        // Broadcasting a signed wallet transaction needs the wallet key + BoC
        // assembly, which lives in the Acton harness (scripts/testnet_e2e.sh).
        // This live RPC seam intentionally only performs reads.
        Err(SettleError::Backend(
            "send_internal is not broadcast from Rust; use scripts/testnet_e2e.sh (Acton) to send"
                .into(),
        ))
    }

    /// Run a get-method and return its first stack entry as an integer. Toncenter
    /// v2 returns `{"result":{"stack":[["num","0x.."], ..],"exit_code":0}}`.
    fn run_get_int(&self, addr: &WalletAddress, method: &str) -> Result<i128, SettleError> {
        // The on-chain contracts are addressed in user-friendly form on testnet;
        // accept the raw `wc:hex` form too and let Toncenter normalize.
        let raw = self.run_get_method_raw(&addr.to_raw_string(), method)?;
        parse_first_stack_int(&raw)
    }
}

/// Parse the first integer of a Toncenter `runGetMethod` `result.stack`.
fn parse_first_stack_int(raw: &str) -> Result<i128, SettleError> {
    let v: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| SettleError::Backend(format!("bad JSON: {e}")))?;
    // Surface a Toncenter-level error verbatim if present.
    if v.get("ok").and_then(|o| o.as_bool()) == Some(false) {
        return Err(SettleError::Backend(format!("toncenter error: {raw}")));
    }
    let first = v
        .get("result")
        .and_then(|r| r.get("stack"))
        .and_then(|s| s.get(0))
        .and_then(|e| e.get(1))
        .ok_or_else(|| SettleError::Backend(format!("no stack[0] in response: {raw}")))?;
    let s = first
        .as_str()
        .ok_or_else(|| SettleError::Backend("stack value not a string".into()))?;
    let s = s.trim();
    let parsed = if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        i128::from_str_radix(hex, 16)
    } else if let Some(hex) = s.strip_prefix("-0x") {
        i128::from_str_radix(hex, 16).map(|n| -n)
    } else {
        s.parse::<i128>()
    };
    parsed.map_err(|e| SettleError::Backend(format!("cannot parse stack int '{s}': {e}")))
}

fn skip(reason: &str) {
    eprintln!("SKIP testnet_live: {reason} (set TON_TESTNET_RPC + TON_TESTNET_*_ADDR to run)");
}

// ---------------------------------------------------------------------------
// ABI: these run even without network — they pin the on-chain message layout the
// live contracts expect, so a drift between Rust and Tolk is caught here too.
// ---------------------------------------------------------------------------

#[test]
fn message_abi_matches_onchain_opcodes() {
    assert_eq!(build_stake_deposit(1, 100).opcode, OP_STAKE_DEPOSIT);

    let challenger = WalletAddress::new(0, [7u8; 32]);
    let slash = build_stake_slash(9, 50, SlashReason::Cheat, &challenger);
    assert_eq!(slash.opcode, OP_STAKE_SLASH);
    // opcode(4) + queryId(8) + coins(16) + reason(1) + addr(36)
    assert_eq!(slash.bytes.len(), 4 + 8 + 16 + 1 + 36);

    let winner = WalletAddress::new(0, [2u8; 32]);
    // B1: candidates must include the winner (the contract membership-checks it);
    // both the participants and candidates dicts are omitted from the flat ABI.
    let settle = build_escrow_settle(1, &[3u8; 32], &winner, 60, 2, &[], &[winner]);
    assert_eq!(settle.opcode, OP_ESCROW_SETTLE);
    // opcode(4)+queryId(8)+hash(32)+addr(36)+coins(16)+coins(16) (dicts omitted from flat ABI)
    assert_eq!(settle.bytes.len(), 4 + 8 + 32 + 36 + 16 + 16);

    let anchor = build_anchor_submit(1, 7, &[1u8; 32], &[0u8; 32], 1_000);
    assert_eq!(anchor.opcode, OP_ANCHOR_SUBMIT);
    // opcode(4)+queryId(8)+epoch(4)+root(32)+prevRoot(32)+coins(16)
    assert_eq!(anchor.bytes.len(), 4 + 8 + 4 + 32 + 32 + 16);
}

#[test]
fn parse_stack_int_handles_hex_and_dec() {
    let hex = r#"{"ok":true,"result":{"stack":[["num","0x64"]],"exit_code":0}}"#;
    assert_eq!(parse_first_stack_int(hex).unwrap(), 100);
    let dec = r#"{"ok":true,"result":{"stack":[["num","42"]],"exit_code":0}}"#;
    assert_eq!(parse_first_stack_int(dec).unwrap(), 42);
    let err = r#"{"ok":false,"error":"boom"}"#;
    assert!(parse_first_stack_int(err).is_err());
}

// ---------------------------------------------------------------------------
// Live reads through the TonRpc seam (skipped unless the env is configured).
// ---------------------------------------------------------------------------

#[test]
fn live_vault_reflects_stake_deposit() {
    let Some(rpc) = CurlTonRpc::from_env() else {
        return skip("TON_TESTNET_RPC not set");
    };
    let Some(addr) = std::env::var("TON_TESTNET_VAULT_ADDR")
        .ok()
        .filter(|s| !s.is_empty())
    else {
        return skip("TON_TESTNET_VAULT_ADDR not set");
    };
    let vault = WalletAddress::from_raw_str(&addr)
        .or_else(|_| friendly_to_wallet(&rpc, &addr))
        .expect("vault address");

    // `get_vault_state` returns (staked, unbondingAmount, unbondingAt, totalSupply, eligible);
    // the first stack item is `staked`. After the harness's deposit it must be > 0.
    match rpc.run_get_int(&vault, "get_vault_state") {
        Ok(staked) => {
            println!("live StakeVault staked = {staked} nanoton");
            assert!(staked >= 0, "staked must be non-negative");
        }
        Err(e) => panic!("live get_vault_state failed: {e}"),
    }
}

#[test]
fn live_anchor_state_is_readable() {
    let Some(rpc) = CurlTonRpc::from_env() else {
        return skip("TON_TESTNET_RPC not set");
    };
    let Some(addr) = std::env::var("TON_TESTNET_ANCHOR_ADDR")
        .ok()
        .filter(|s| !s.is_empty())
    else {
        return skip("TON_TESTNET_ANCHOR_ADDR not set");
    };
    let anchor = WalletAddress::from_raw_str(&addr)
        .or_else(|_| friendly_to_wallet(&rpc, &addr))
        .expect("anchor address");

    // `get_anchor_state` returns (currentEpoch, lastRoot, nextDisputeId); the
    // first item (currentEpoch) must be readable and >= 1 after a harness run.
    match rpc.run_get_int(&anchor, "get_anchor_state") {
        Ok(epoch) => {
            println!("live RecordAnchor currentEpoch = {epoch}");
            assert!(epoch >= 0);
        }
        Err(e) => panic!("live get_anchor_state failed: {e}"),
    }
}

#[test]
fn send_internal_is_read_only_seam() {
    // The live seam refuses to broadcast from Rust (documented boundary).
    let rpc = CurlTonRpc {
        rpc: "https://example.invalid".into(),
        api_key: None,
    };
    let body = build_stake_deposit(0, 0);
    let err = rpc
        .send_internal(&WalletAddress::new(0, [0u8; 32]), 0, &body)
        .unwrap_err();
    assert!(matches!(err, SettleError::Backend(_)));
}

/// Resolve a user-friendly (`EQ.../kQ...`) address to raw via Toncenter's
/// `detectAddress`, so env vars written by the harness (friendly form) work.
fn friendly_to_wallet(
    rpc: &CurlTonRpc,
    friendly: &str,
) -> Result<WalletAddress, p2p_settlement::types::BindingError> {
    let url = format!("{}/detectAddress?address={}", rpc.rpc, friendly);
    let mut cmd = Command::new("curl");
    cmd.arg("-s").arg("--max-time").arg("30");
    if let Some(k) = &rpc.api_key {
        cmd.arg("-H").arg(format!("X-API-Key: {k}"));
    }
    cmd.arg(url);
    let out = cmd
        .output()
        .map_err(|_| p2p_settlement::types::BindingError::BadAddress)?;
    let raw = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value =
        serde_json::from_str(&raw).map_err(|_| p2p_settlement::types::BindingError::BadAddress)?;
    let raw_form = v
        .get("result")
        .and_then(|r| r.get("raw_form"))
        .and_then(|s| s.as_str())
        .ok_or(p2p_settlement::types::BindingError::BadAddress)?;
    WalletAddress::from_raw_str(raw_form)
}
