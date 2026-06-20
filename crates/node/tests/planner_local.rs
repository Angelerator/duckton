//! Integration tests for the local-first execution path + the data-size
//! estimator + the local-vs-remote planner (architecture §4 / §11).
//!
//! These run on the deterministic mock engine + pure-Rust metadata readers, so
//! they need NO real DuckDB engine (the bundled C++ build is unavailable in
//! this environment). Engine-backed estimator probes (`parquet_metadata()`,
//! `EXPLAIN`) live behind the `duckdb-engine` feature — see
//! `planner_local_duckdb.rs`. The estimator *core* and CSV/Delta readers are
//! exercised here against real local fixtures + synthetic Parquet metadata that
//! mirrors the documented `parquet_metadata()` column semantics.

use std::sync::Arc;

use p2p_config::{GridConfig, IdentityConfig, PinningMode, PreferMode, QueryOverrides};
use p2p_node::{
    csv_metadata, delta_metadata, estimate_table_files, estimate_text, estimate_working_set, Cmp,
    Coordinator, CoordinatorError, DefaultPlanner, ExecLease, LocalExecutor, LocalOrRemotePlanner,
    MockEngine, Predicate, Projection, QueryEngine, QueryShape, ScanEstimate, StaticDiscovery,
    WorkingSetEstimate,
};
use p2p_transport::{NodeIdentity, QuicTransport};
use p2p_trust::InMemoryTrustStore;

// ---------------------------------------------------------------------------
// Estimator accuracy — real local fixtures, no engine
// ---------------------------------------------------------------------------

#[test]
fn csv_estimator_accuracy_on_local_fixture() {
    // A CSV with a header + 1000 data rows of (mostly) uniform width.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.csv");
    let mut text = String::from("region,user_id,amount\n");
    for i in 0..1000 {
        text.push_str(&format!(
            "us-east-{:03},{:06},{}\n",
            i % 100,
            i,
            (i * 7) % 1000
        ));
    }
    let actual_rows = 1000u64;
    std::fs::write(&path, &text).unwrap();

    // Sample the whole file (sample limit >= file size) → exact average width.
    let meta = csv_metadata(&path, b',', 1 << 20).unwrap();
    assert_eq!(meta.total_columns, 3);
    assert_eq!(meta.object_bytes, text.len() as u64);

    let params = Default::default();
    // Full projection.
    let scan = estimate_text(&meta, &Projection::All, &[], &params);
    // Row-count estimate from object_bytes / avg_row_width must be within 2% of
    // the true 1000 rows (uniform-ish widths → tight).
    let err = (scan.total_rows as i64 - actual_rows as i64).abs() as f64 / actual_rows as f64;
    assert!(err < 0.02, "row estimate {} off by {err}", scan.total_rows);

    // Projection pushdown: 1 of 3 columns ≈ a third of the bytes.
    let projected = estimate_text(&meta, &Projection::columns(["amount"]), &[], &params);
    assert!(
        projected.scanned_uncompressed_bytes < scan.scanned_uncompressed_bytes,
        "projection should reduce scanned bytes"
    );
    let ratio =
        projected.scanned_uncompressed_bytes as f64 / scan.scanned_uncompressed_bytes as f64;
    assert!(
        (0.28..0.39).contains(&ratio),
        "projection ratio was {ratio}"
    );

    // A streaming SELECT amount has a tiny working set (bounded scan buffer).
    let ws = estimate_working_set(&projected, &QueryShape::streaming(), &params);
    assert_eq!(ws.group_by_bytes, 0);
    assert_eq!(ws.peak_working_set_bytes, ws.scan_buffer_bytes);
}

#[test]
fn delta_log_estimator_pure_rust_with_pruning() {
    // Build a minimal Delta table: _delta_log/0000.json with a metaData schema
    // and two add actions, one of which is later removed.
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("_delta_log");
    std::fs::create_dir_all(&log).unwrap();

    let schema = r#"{"type":"struct","fields":[
        {"name":"ts","type":"long","nullable":true,"metadata":{}},
        {"name":"region","type":"string","nullable":true,"metadata":{}},
        {"name":"amount","type":"double","nullable":true,"metadata":{}},
        {"name":"note","type":"string","nullable":true,"metadata":{}}
    ]}"#;
    let schema_escaped = serde_json::to_string(schema).unwrap();

    let commit = format!(
        concat!(
            "{{\"metaData\":{{\"schemaString\":{schema}}}}}\n",
            "{{\"add\":{{\"path\":\"a.parquet\",\"size\":1000000,\"stats\":\"{{\\\"numRecords\\\":10000,\\\"minValues\\\":{{\\\"ts\\\":0}},\\\"maxValues\\\":{{\\\"ts\\\":100}}}}\"}}}}\n",
            "{{\"add\":{{\"path\":\"b.parquet\",\"size\":1000000,\"stats\":\"{{\\\"numRecords\\\":10000,\\\"minValues\\\":{{\\\"ts\\\":200}},\\\"maxValues\\\":{{\\\"ts\\\":300}}}}\"}}}}\n",
            "{{\"add\":{{\"path\":\"c.parquet\",\"size\":1000000,\"stats\":\"{{\\\"numRecords\\\":10000,\\\"minValues\\\":{{\\\"ts\\\":400}},\\\"maxValues\\\":{{\\\"ts\\\":500}}}}\"}}}}\n",
            "{{\"remove\":{{\"path\":\"c.parquet\"}}}}\n"
        ),
        schema = schema_escaped
    );
    std::fs::write(log.join("00000000000000000000.json"), commit).unwrap();

    let meta = delta_metadata(dir.path()).unwrap();
    assert_eq!(meta.all_columns, vec!["ts", "region", "amount", "note"]);
    // c.parquet was removed → only a + b remain.
    assert_eq!(meta.files.len(), 2);

    let params = Default::default();
    // Filter ts < 150 → prunes b (min 200); only a survives.
    let scan = estimate_table_files(
        &meta,
        &Projection::columns(["ts", "amount"]),
        &[Predicate::new("ts", Cmp::Lt, 150.0)],
        &params,
    );
    assert_eq!(scan.units_scanned, 1);
    assert_eq!(scan.total_rows, 10_000);
    // 2 of 4 columns projected, 4x decompression of 1MB on-disk → 2MB.
    assert_eq!(scan.scanned_uncompressed_bytes, 2_000_000);
}

// ---------------------------------------------------------------------------
// Coordinator harness (no workers → "remote" routing fails with NoCandidates,
// which is exactly how we prove a query was NOT run locally)
// ---------------------------------------------------------------------------

fn idcfg() -> IdentityConfig {
    IdentityConfig {
        key_path: None,
        pinning_mode: PinningMode::Tofu,
        allowlist: vec![],
    }
}

/// A coordinator with NO grid workers but WITH a local-execution path. Returns
/// the coordinator + a handle to the same `LocalExecutor` so a test can reserve
/// slots to simulate saturation.
fn local_coordinator(
    engine: Arc<dyn QueryEngine>,
    cfg: GridConfig,
) -> (Coordinator, Arc<LocalExecutor>) {
    let net = GridConfig::default().network;
    let transport =
        Arc::new(QuicTransport::bind(&net, &idcfg(), NodeIdentity::generate().unwrap()).unwrap());
    let disc = Arc::new(StaticDiscovery::new(
        vec![],
        cfg.discovery.candidate_sample_size,
    ));
    let store = Arc::new(InMemoryTrustStore::new(&cfg.trust, &cfg.limits));
    let local = LocalExecutor::new(engine, cfg.budget.memory_bytes, &cfg.planner);
    let planner: Arc<dyn LocalOrRemotePlanner> = Arc::new(DefaultPlanner::new(cfg.planner.clone()));
    let coord = Coordinator::new(transport, disc, store, Arc::new(cfg), "mock-1")
        .with_local_execution(Arc::clone(&local), planner);
    (coord, local)
}

fn fitting_estimate() -> WorkingSetEstimate {
    let scan = ScanEstimate {
        scanned_uncompressed_bytes: 1_000_000,
        total_rows: 10_000,
        estimated_output_rows: 10_000,
        avg_row_width_bytes: 100,
        units_total: 1,
        units_scanned: 1,
        projected_columns: 2,
    };
    estimate_working_set(&scan, &QueryShape::streaming(), &Default::default())
}

// ---------------------------------------------------------------------------
// Local path + routing decisions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn prefer_local_runs_free_local_path() {
    let (coord, _local) =
        local_coordinator(Arc::new(MockEngine::deterministic()), GridConfig::default());
    let sql = "SELECT region, count(*) FROM 's3://b/e/*.parquet' GROUP BY region";

    let overrides = QueryOverrides {
        prefer: Some(PreferMode::Local),
        ..Default::default()
    };
    let outcome = coord.run_query(sql, overrides).await.unwrap();

    // Free local path: ran locally, trusted, no quorum, no receipts/payment.
    assert!(outcome.executed_locally);
    assert!(outcome.verified);
    assert!(outcome.receipts.is_empty());
    assert_eq!(outcome.quorum, 0);

    // Result equals the same deterministic engine computed independently.
    let expected = MockEngine::deterministic()
        .execute(
            sql,
            ExecLease {
                memory_bytes: 1 << 20,
                threads: 1,
                max_spill_bytes: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(outcome.result, expected);
}

#[tokio::test]
async fn prefer_remote_bypasses_local_and_dispatches_to_grid() {
    let (coord, _local) =
        local_coordinator(Arc::new(MockEngine::deterministic()), GridConfig::default());
    let overrides = QueryOverrides {
        prefer: Some(PreferMode::Remote),
        ..Default::default()
    };
    // No workers exist → going remote must surface NoCandidates (proving it did
    // NOT silently run locally).
    let err = coord.run_query("SELECT 1", overrides).await.unwrap_err();
    assert!(matches!(err, CoordinatorError::NoCandidates), "got {err:?}");
}

#[tokio::test]
async fn auto_fitting_estimate_runs_local() {
    let (coord, _local) =
        local_coordinator(Arc::new(MockEngine::deterministic()), GridConfig::default());
    let outcome = coord
        .run_query_planned(
            "SELECT 1",
            QueryOverrides::default(),
            Some(fitting_estimate()),
        )
        .await
        .unwrap();
    assert!(outcome.executed_locally);
}

#[tokio::test]
async fn auto_too_big_estimate_goes_remote() {
    let mut cfg = GridConfig::default();
    // Tiny local budget so the (1MB scan, capped) estimate can't fit, and no
    // spill tolerance.
    cfg.budget.memory_bytes = 256 * 1024; // 256 KiB total
    cfg.planner.ram_fraction = 1.0;
    cfg.planner.spill_tolerance_bytes = 0;
    cfg.validate().unwrap();

    let (coord, _local) = local_coordinator(Arc::new(MockEngine::deterministic()), cfg);
    // Estimate peak ~1MB >> 256KiB headroom → planner routes remote → no workers
    // → NoCandidates.
    let err = coord
        .run_query_planned(
            "SELECT 1",
            QueryOverrides::default(),
            Some(fitting_estimate()),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CoordinatorError::NoCandidates), "got {err:?}");
}

#[tokio::test]
async fn locally_saturated_falls_back_to_remote() {
    let mut cfg = GridConfig::default();
    cfg.planner.max_concurrent_local_jobs = 1;
    cfg.validate().unwrap();

    let (coord, local) = local_coordinator(Arc::new(MockEngine::deterministic()), cfg);
    // Occupy the single local slot.
    let _held = local.reserve(0).expect("first reservation succeeds");
    assert!(!local.slot_available());

    // A fitting estimate would normally go local, but the slot is taken → remote
    // → NoCandidates.
    let err = coord
        .run_query_planned(
            "SELECT 1",
            QueryOverrides::default(),
            Some(fitting_estimate()),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CoordinatorError::NoCandidates), "got {err:?}");
}

#[tokio::test]
async fn adaptive_failover_redispatches_to_grid_on_oom() {
    // The local engine blows up mid-flight (simulated OOM). With prefer=auto and
    // a fitting estimate the planner first chooses local, the local run fails
    // with a resource-exhaustion error, and the coordinator fails over to the
    // grid (which, lacking workers, surfaces NoCandidates — proving the
    // re-dispatch happened rather than the local error being returned).
    let oom = Arc::new(MockEngine::failing(
        "Out of Memory Error: failed to allocate 8GB",
    ));
    let (coord, _local) = local_coordinator(oom, GridConfig::default());

    let err = coord
        .run_query_planned(
            "SELECT 1",
            QueryOverrides::default(),
            Some(fitting_estimate()),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, CoordinatorError::NoCandidates),
        "expected failover to grid, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Remote-only ("route everything to the grid; never execute locally") mode
// ---------------------------------------------------------------------------

/// Build a GridConfig with local execution disabled (remote-only mode).
fn remote_only_cfg() -> GridConfig {
    let mut cfg = GridConfig::default();
    cfg.planner.local_execution_enabled = false;
    cfg.validate().unwrap();
    cfg
}

#[tokio::test]
async fn remote_only_mode_dispatches_tiny_fitting_query_to_grid() {
    // A tiny query that WOULD fit locally must still be dispatched to the grid
    // when local execution is disabled. With no workers, going remote surfaces
    // NoCandidates — proving the local path was NOT taken.
    let (coord, local) =
        local_coordinator(Arc::new(MockEngine::deterministic()), remote_only_cfg());
    // Sanity: a local slot IS available and the estimate fits, so the ONLY reason
    // we go remote is the remote-only hard gate (not saturation / size).
    assert!(local.slot_available());

    let err = coord
        .run_query_planned(
            "SELECT 1",
            QueryOverrides::default(),
            Some(fitting_estimate()),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CoordinatorError::NoCandidates), "got {err:?}");
}

#[tokio::test]
async fn remote_only_mode_hard_gate_overrides_per_call_prefer_local() {
    // Even an explicit `prefer => local` cannot make a remote-only node run a
    // query on its own machine: the hard gate wins → grid → NoCandidates.
    let (coord, _local) =
        local_coordinator(Arc::new(MockEngine::deterministic()), remote_only_cfg());
    let overrides = QueryOverrides {
        prefer: Some(PreferMode::Local),
        ..Default::default()
    };
    let err = coord.run_query("SELECT 1", overrides).await.unwrap_err();
    assert!(matches!(err, CoordinatorError::NoCandidates), "got {err:?}");
}

#[tokio::test]
async fn remote_only_mode_skips_adaptive_failover_start_local_path() {
    // The local engine would fail with OOM IF it ran — but in remote-only mode
    // the "start local" path is skipped entirely, so the engine is never invoked
    // and we go straight to the grid (NoCandidates). A LocalExecution error here
    // would mean the local path ran, which must not happen.
    let oom = Arc::new(MockEngine::failing(
        "Out of Memory Error: failed to allocate 8GB",
    ));
    let (coord, _local) = local_coordinator(oom, remote_only_cfg());
    let err = coord
        .run_query_planned(
            "SELECT 1",
            QueryOverrides::default(),
            Some(fitting_estimate()),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CoordinatorError::NoCandidates), "got {err:?}");
}

#[tokio::test]
async fn thin_client_remote_only_dispatches_and_surfaces_no_candidates() {
    // A thin-client requester: never called p2p_share (no worker wired here) and
    // local execution disabled. `p2p_query` must dispatch to hosts (no dependency
    // on a local executor) and, with no reachable peers, surface NoCandidates
    // cleanly rather than running locally.
    let (coord, _local) =
        local_coordinator(Arc::new(MockEngine::deterministic()), remote_only_cfg());
    // Default overrides → planner.prefer = auto, but the hard gate forces remote.
    let err = coord
        .run_query(
            "SELECT region, count(*) FROM 's3://b/e/*.parquet' GROUP BY region",
            QueryOverrides::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CoordinatorError::NoCandidates), "got {err:?}");
    // The actionable message points at p2p_join / bootstrap seeds.
    let msg = err.to_string();
    assert!(
        msg.contains("p2p_join") || msg.contains("bootstrap"),
        "unfriendly: {msg}"
    );
}

#[tokio::test]
async fn prefer_local_does_not_failover_and_surfaces_local_error() {
    // When the caller pins `local`, a local failure is NOT masked by a grid
    // re-dispatch — it is surfaced as a LocalExecution error.
    let oom = Arc::new(MockEngine::failing("Out of Memory Error"));
    let (coord, _local) = local_coordinator(oom, GridConfig::default());

    let overrides = QueryOverrides {
        prefer: Some(PreferMode::Local),
        ..Default::default()
    };
    let err = coord.run_query("SELECT 1", overrides).await.unwrap_err();
    assert!(
        matches!(err, CoordinatorError::LocalExecution(_)),
        "got {err:?}"
    );
}
