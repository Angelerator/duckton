//! Zero-config "just works" requester experience (architecture §12 SQL surface,
//! §17 config layering).
//!
//! These prove the frictionless default path: a user runs `p2p_query(...)` with
//! NO prior `p2p_join`/`p2p_share`, NO config file and NO env vars, and the node
//! lazily auto-initializes with safe built-in defaults and runs the query
//! locally for free. They also prove the customization layers (per-call params,
//! config file, env) still apply on top. All run on the deterministic mock
//! engine — no real DuckDB engine required.

use std::collections::BTreeMap;
use std::sync::Arc;

use p2p_config::{GridConfig, PaymentPref, PreferMode, QueryOverrides};
use p2p_node::{CoordinatorError, ExecLease, MockEngine, Node, NodeError, QueryEngine};

fn engine() -> Arc<dyn QueryEngine> {
    Arc::new(MockEngine::deterministic())
}

// ---------------------------------------------------------------------------
// Zero-config: no file, no env, no prior join/share → runs local-first & free.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn zero_config_query_just_works() {
    // `GridConfig::default()` IS the "no config file, no env vars" state — the
    // built-in defaults layer with nothing on top.
    let node = Node::with_config(GridConfig::default(), engine()).unwrap();

    let sql = "SELECT region, count(*) FROM 's3://bucket/events/*.parquet' GROUP BY region";
    // The minimal call a user writes: just the SQL, no overrides, no setup.
    let outcome = node.query(sql, QueryOverrides::default()).await.unwrap();

    // Auto-initialized + ran on the free local path (no grid, no payment).
    assert!(outcome.executed_locally, "should run locally with no seeds");
    assert!(outcome.verified, "own machine is trusted");
    assert!(outcome.receipts.is_empty(), "free local path emits no receipts");
    assert_eq!(outcome.quorum, 0);

    // Result equals the same deterministic engine computed independently.
    let expected = MockEngine::deterministic()
        .execute(sql, ExecLease { memory_bytes: 1 << 20, threads: 1 })
        .await
        .unwrap();
    assert_eq!(outcome.result, expected);
}

#[tokio::test]
async fn auto_loads_built_in_defaults_with_no_env_or_file() {
    // `Node::auto` runs the full defaults → file → env load. With no P2P_CONFIG
    // and no P2P_* env set (the test process), it resolves to the defaults and
    // still "just works" local-first.
    let node = Node::auto(engine()).unwrap();
    let outcome = node.query("SELECT 1", QueryOverrides::default()).await.unwrap();
    assert!(outcome.executed_locally);
}

// ---------------------------------------------------------------------------
// Customization via per-call params still works (overrides change behavior).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn per_call_prefer_remote_overrides_default_and_dispatches() {
    // No seeds configured, but the user explicitly asks to run on the grid.
    // The default would have run local-first; the override flips it to remote,
    // which (lacking any worker) surfaces NoCandidates — proving the per-call
    // param actually changed behavior rather than silently running local.
    let node = Node::with_config(GridConfig::default(), engine()).unwrap();
    let err = node
        .query(
            "SELECT 1",
            QueryOverrides {
                prefer: Some(PreferMode::Remote),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, NodeError::Query(CoordinatorError::NoCandidates)),
        "got {err:?}"
    );
}

#[tokio::test]
async fn per_call_prefer_local_forces_free_local_even_with_seeds() {
    // A grid IS configured (a bogus, unreachable seed), so the default `auto`
    // would route remote. `prefer => local` forces the free local path instead.
    let mut cfg = GridConfig::default();
    cfg.discovery.bootstrap = vec!["quic://127.0.0.1:1".to_string()];
    let node = Node::with_config(cfg, engine()).unwrap();

    let outcome = node
        .query(
            "SELECT 1",
            QueryOverrides {
                prefer: Some(PreferMode::Local),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(outcome.executed_locally);
}

#[tokio::test]
async fn auto_with_unreachable_grid_falls_back_to_local() {
    // Seeds configured but unreachable; `auto` tries the grid, finds nothing,
    // and gracefully falls back to the free local path rather than erroring.
    let mut cfg = GridConfig::default();
    cfg.discovery.bootstrap = vec!["quic://127.0.0.1:1".to_string()];
    let node = Node::with_config(cfg, engine()).unwrap();

    let outcome = node.query("SELECT 1", QueryOverrides::default()).await.unwrap();
    assert!(outcome.executed_locally, "auto should fall back to local");
}

// ---------------------------------------------------------------------------
// Remote-only ("route everything to the grid") / thin-client mode at the Node.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn remote_only_node_does_not_fall_back_to_local() {
    // Local execution disabled. Unlike the local-first default, a thin-client
    // requester with no reachable grid must NOT silently run locally — it
    // surfaces NoCandidates so the caller knows to join a network.
    let mut cfg = GridConfig::default();
    cfg.planner.local_execution_enabled = false;
    cfg.validate().unwrap();
    let node = Node::with_config(cfg, engine()).unwrap();

    let err = node
        .query("SELECT 1", QueryOverrides::default())
        .await
        .unwrap_err();
    assert!(
        matches!(err, NodeError::Query(CoordinatorError::NoCandidates)),
        "remote-only must not fall back to local; got {err:?}"
    );
}

#[tokio::test]
async fn remote_only_node_ignores_per_call_prefer_local() {
    // The hard gate beats a per-call `prefer => local`: a remote-only node never
    // executes locally, so this dispatches to the (empty) grid → NoCandidates.
    let mut cfg = GridConfig::default();
    cfg.planner.local_execution_enabled = false;
    cfg.validate().unwrap();
    let node = Node::with_config(cfg, engine()).unwrap();

    let err = node
        .query(
            "SELECT 1",
            QueryOverrides {
                prefer: Some(PreferMode::Local),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, NodeError::Query(CoordinatorError::NoCandidates)),
        "got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Friendly error: paid execution without a wallet → actionable message.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn paid_without_wallet_returns_friendly_error() {
    // Turn the chain on so a `paid` request actually resolves to PAID.
    let mut cfg = GridConfig::default();
    cfg.economics.enabled = true; // settlement stays noop (valid)
    cfg.validate().unwrap();
    let node = Node::with_config(cfg, engine()).unwrap();

    let err = node
        .query(
            "SELECT 1",
            QueryOverrides {
                payment: Some(PaymentPref::Paid),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, NodeError::WalletRequired), "got {err:?}");
    // The message points the user at the actionable override.
    assert!(err.to_string().contains("payment => 'free'"));
}

#[tokio::test]
async fn paid_with_wallet_passes_the_gate() {
    // Same config, but a wallet/stake registry is attached → the gate lifts.
    let mut cfg = GridConfig::default();
    cfg.economics.enabled = true;
    cfg.validate().unwrap();
    let node = Node::with_config(cfg, engine())
        .unwrap()
        .with_wallet(Arc::new(p2p_settlement::NoopStakeRegistry));

    // With no grid it still runs local-first (local is free); the point is that
    // the WalletRequired gate did NOT trip.
    let outcome = node
        .query(
            "SELECT 1",
            QueryOverrides {
                payment: Some(PaymentPref::Paid),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(outcome.executed_locally);
}

// ---------------------------------------------------------------------------
// Config-file + env overrides still layer through to node behavior.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn config_file_then_env_overrides_apply() {
    // Simulate the file layer (TOML) asking for remote, then the env layer
    // flipping it back to local — proving both layers reach the node. We use the
    // testable `apply_env_map` so we don't mutate the process environment.
    let mut cfg = GridConfig::from_toml_str("[planner]\nprefer = \"remote\"\n").unwrap();
    assert_eq!(cfg.planner.prefer, PreferMode::Remote);

    let mut env = BTreeMap::new();
    env.insert("P2P_PLANNER_PREFER".to_string(), "local".to_string());
    cfg.apply_env_map(&env).unwrap();
    assert_eq!(cfg.planner.prefer, PreferMode::Local);
    cfg.validate().unwrap();

    let node = Node::with_config(cfg, engine()).unwrap();
    // No per-call override → inherits the resolved config (local) → runs local.
    let outcome = node.query("SELECT 1", QueryOverrides::default()).await.unwrap();
    assert!(outcome.executed_locally);
}
