//! Engine-backed estimator tests (architecture §4 / estimator) — REAL DuckDB.
//!
//! Gated behind the `duckdb-engine` feature, which bundles + compiles DuckDB
//! from source. NOTE: in this environment the host C++ toolchain is broken, so
//! this file may not build/run here — that is the documented caveat. When the
//! engine *can* be built, run with:
//!   SDKROOT=$(xcrun --show-sdk-path) \
//!     cargo test -p p2p-node --features duckdb-engine --test planner_local_duckdb
//!
//! These exercise the parts of the estimator that genuinely require an engine:
//!  * reading a real Parquet footer via `parquet_metadata()` and turning it
//!    into a `ParquetMetadata` the estimator core consumes (footer-only, no
//!    full scan), then checking projection + row-group pruning on real stats;
//!  * an `EXPLAIN` cardinality probe on a real plan.
//! We never fabricate engine output — if the engine can't be built this file is
//! simply excluded from the build.
#![cfg(feature = "duckdb-engine")]

use p2p_config::StorageConfig;
use p2p_node::{
    estimate_parquet, estimate_working_set, Cmp, DuckDbEngine, ExecLease, Predicate, Projection,
    QueryEngine, QueryShape,
};

fn lease() -> ExecLease {
    ExecLease {
        memory_bytes: 256 * 1024 * 1024,
        threads: 1,
        max_spill_bytes: 0,
    }
}

fn local_scoped_cfg(dir: &str) -> StorageConfig {
    let mut cfg = StorageConfig::default();
    cfg.allowed_local_paths = vec![dir.to_string()];
    cfg.enable_remote_access = false;
    cfg
}

#[tokio::test]
async fn parquet_footer_probe_drives_estimate_with_pruning() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nums.parquet");
    let eng =
        DuckDbEngine::from_storage_config(&local_scoped_cfg(dir.path().to_str().unwrap())).unwrap();

    // Write a multi-row-group Parquet file: 4000 rows, small row-group size so
    // the footer has several row-groups with id min/max stats for pruning.
    let copy = format!(
        "COPY (SELECT i AS id, i % 1000 AS bucket, repeat('x', 16) AS pad \
         FROM range(4000) t(i)) TO '{}' (FORMAT parquet, ROW_GROUP_SIZE 1000)",
        path.display()
    );
    eng.execute(&copy, lease()).await.unwrap();

    // Footer-only metadata probe (no full scan).
    let meta = eng
        .probe_parquet_metadata(path.to_str().unwrap(), lease())
        .await
        .unwrap();
    assert!(meta.row_groups.len() >= 2, "expected multiple row-groups");

    let params = Default::default();
    // Project only "id"; its uncompressed size should be a small fraction of
    // the full file (which includes the 16-byte `pad` column).
    let proj_id = estimate_parquet(&meta, &Projection::columns(["id"]), &[], &params);
    let proj_all = estimate_parquet(&meta, &Projection::All, &[], &params);
    assert!(proj_id.scanned_uncompressed_bytes < proj_all.scanned_uncompressed_bytes);
    assert_eq!(proj_id.total_rows, 4000);

    // Predicate pushdown via real min/max stats on "id": id >= 3000 prunes the
    // early row-groups.
    let pruned = estimate_parquet(
        &meta,
        &Projection::columns(["id"]),
        &[Predicate::new("id", Cmp::Ge, 3000.0)],
        &params,
    );
    assert!(
        pruned.units_scanned < proj_id.units_scanned,
        "stats should prune row-groups"
    );
    assert!(pruned.total_rows <= 1000 + proj_id.total_rows / meta.row_groups.len() as u64 + 1);

    // A high-cardinality GROUP BY working set dominates the streaming scan.
    let ws_stream = estimate_working_set(&proj_id, &QueryShape::streaming(), &params);
    let ws_group = estimate_working_set(&proj_id, &QueryShape::group_by(4000), &params);
    assert!(ws_group.peak_working_set_bytes > ws_stream.peak_working_set_bytes);
}

#[tokio::test]
async fn explain_cardinality_probe_returns_estimate() {
    let dir = tempfile::tempdir().unwrap();
    let eng =
        DuckDbEngine::from_storage_config(&local_scoped_cfg(dir.path().to_str().unwrap())).unwrap();
    let ec = eng
        .probe_explain_cardinality(
            "SELECT i % 10 AS g, count(*) FROM range(10000) t(i) GROUP BY g",
            lease(),
        )
        .await
        .unwrap();
    // DuckDB annotates the plan with EC: n; we should parse a positive estimate.
    assert!(
        ec.unwrap_or(0) > 0,
        "expected a positive EXPLAIN cardinality, got {ec:?}"
    );
}
