//! TPC-H concurrent big-query load test against the REAL node `DuckDbEngine`
//! (architecture §4 data plane, §10 admission, §11 scheduler).
//!
//! Gated behind the `duckdb-engine` feature AND `#[ignore]` (it reads multi-GB
//! local Parquet and is not a CI unit test). Drive it with:
//!
//!   SDKROOT=$(xcrun --show-sdk-path) \
//!     cargo test -p p2p-node --features duckdb-engine --test tpch_loadtest \
//!     -- --ignored --nocapture
//!
//! Data: TPC-H Parquet at `$TPCH_DATA` (default `~/tpch-data/sf{0.1,1,10}`),
//! one file per table. The engine reads them through its **local-scoped**
//! profile (`allowed_local_paths = [<data root>]`, network egress OFF), exactly
//! like `storage_reads_duckdb::local_scoped_reads_parquet_fixture` — the only
//! file-backed path that works (the extension's HostEngine blocks local reads).
//!
//! Each scenario can be run independently:
//!   * `tpch_smoke_sf01`          — SF0.1 wiring + correctness smoke (all 5 Qs)
//!   * `tpch_engine_serial`       — Q1/Q6/Q3/Q5/Q18 serial @ SF1 & SF10
//!   * `tpch_concurrency`         — 8/16/32 concurrent via AdmissionController
//!   * `tpch_spill`               — SF10 Q1/Q18 under a 512MB / 256MB memory_limit
//!   * `tpch_grid_loopback`       — grid path (prefer=>remote) over loopback QUIC
//!
//! `TPCH_SF` (default `sf1,sf10`) selects which scale factors the serial/spill
//! scenarios touch so a quick run can stick to SF1.
#![cfg(feature = "duckdb-engine")]

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use p2p_config::StorageConfig;
use p2p_node::{DuckDbEngine, ExecLease, QueryEngine};
use p2p_proto::ResultSet;

// --------------------------------------------------------------------------
// Data location + skip handling
// --------------------------------------------------------------------------

/// The TPC-H data root (parent of the `sf*` dirs). `$TPCH_DATA` overrides the
/// `~/tpch-data` default.
fn data_root() -> String {
    if let Ok(d) = std::env::var("TPCH_DATA") {
        return d;
    }
    let home = std::env::var("HOME").expect("HOME is set");
    format!("{home}/tpch-data")
}

/// Returns the SF dir path if it exists, else `None` (caller prints a skip).
fn sf_dir(root: &str, sf: &str) -> Option<String> {
    let p = format!("{root}/{sf}");
    if Path::new(&format!("{p}/lineitem.parquet")).exists() {
        Some(p)
    } else {
        None
    }
}

/// Scale factors selected by `$TPCH_SF` (comma list), default `sf1,sf10`.
fn selected_sfs() -> Vec<String> {
    std::env::var("TPCH_SF")
        .unwrap_or_else(|_| "sf1,sf10".to_string())
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

// --------------------------------------------------------------------------
// Engine construction (local-scoped: allowed dir + network OFF)
// --------------------------------------------------------------------------

/// A local-scoped storage config that allows reading the TPC-H data root and
/// keeps network egress disabled — the proven file-backed profile.
fn local_scoped_cfg(root: &str) -> StorageConfig {
    let mut cfg = StorageConfig::default();
    cfg.allowed_local_paths = vec![root.to_string()];
    cfg.enable_remote_access = false;
    cfg
}

fn engine_for(root: &str) -> DuckDbEngine {
    DuckDbEngine::from_storage_config(&local_scoped_cfg(root)).expect("build DuckDbEngine")
}

fn lease(mem_bytes: u64, threads: u32) -> ExecLease {
    ExecLease {
        memory_bytes: mem_bytes,
        threads,
        max_spill_bytes: 0,
    }
}

// --------------------------------------------------------------------------
// TPC-H queries (DuckDB dialect). Each run opens a FRESH :memory: connection
// (run_locked), so views do not persist across calls — every query inlines its
// tables as `read_parquet(...)` CTEs.
// --------------------------------------------------------------------------

/// Build a `WITH <table> AS (SELECT * FROM read_parquet('<dir>/<table>.parquet'))`
/// prelude for the named tables, so the standard TPC-H SQL body can use bare
/// table names.
fn cte_prelude(dir: &str, tables: &[&str]) -> String {
    let parts: Vec<String> = tables
        .iter()
        .map(|t| format!("{t} AS (SELECT * FROM read_parquet('{dir}/{t}.parquet'))"))
        .collect();
    format!("WITH {} ", parts.join(", "))
}

/// The five "big" queries keyed by name, returning a single-statement SQL with
/// the read_parquet CTE prelude bound for `dir`.
fn tpch_sql(dir: &str, name: &str) -> String {
    match name {
        "Q1" => {
            cte_prelude(dir, &["lineitem"])
                + "SELECT l_returnflag, l_linestatus,
                       sum(l_quantity) AS sum_qty, sum(l_extendedprice) AS sum_base_price,
                       sum(l_extendedprice*(1-l_discount)) AS sum_disc_price,
                       sum(l_extendedprice*(1-l_discount)*(1+l_tax)) AS sum_charge,
                       avg(l_quantity) AS avg_qty, avg(l_extendedprice) AS avg_price,
                       avg(l_discount) AS avg_disc, count(*) AS count_order
                 FROM lineitem
                 WHERE l_shipdate <= DATE '1998-12-01' - INTERVAL '90' DAY
                 GROUP BY l_returnflag, l_linestatus
                 ORDER BY l_returnflag, l_linestatus"
        }
        "Q6" => {
            cte_prelude(dir, &["lineitem"])
                + "SELECT sum(l_extendedprice*l_discount) AS revenue
                 FROM lineitem
                 WHERE l_shipdate >= DATE '1994-01-01'
                   AND l_shipdate <  DATE '1994-01-01' + INTERVAL '1' YEAR
                   AND l_discount BETWEEN 0.06 - 0.01 AND 0.06 + 0.01
                   AND l_quantity < 24"
        }
        "Q3" => {
            cte_prelude(dir, &["customer", "orders", "lineitem"])
                + "SELECT l_orderkey, sum(l_extendedprice*(1-l_discount)) AS revenue,
                          o_orderdate, o_shippriority
                 FROM customer, orders, lineitem
                 WHERE c_mktsegment = 'BUILDING'
                   AND c_custkey = o_custkey AND l_orderkey = o_orderkey
                   AND o_orderdate < DATE '1995-03-15' AND l_shipdate > DATE '1995-03-15'
                 GROUP BY l_orderkey, o_orderdate, o_shippriority
                 ORDER BY revenue DESC, o_orderdate
                 LIMIT 10"
        }
        "Q5" => {
            cte_prelude(
                dir,
                &[
                    "customer", "orders", "lineitem", "supplier", "nation", "region",
                ],
            ) + "SELECT n_name, sum(l_extendedprice*(1-l_discount)) AS revenue
                 FROM customer, orders, lineitem, supplier, nation, region
                 WHERE c_custkey = o_custkey AND l_orderkey = o_orderkey
                   AND l_suppkey = s_suppkey AND c_nationkey = s_nationkey
                   AND s_nationkey = n_nationkey AND n_regionkey = r_regionkey
                   AND r_name = 'ASIA'
                   AND o_orderdate >= DATE '1994-01-01'
                   AND o_orderdate <  DATE '1994-01-01' + INTERVAL '1' YEAR
                 GROUP BY n_name
                 ORDER BY revenue DESC"
        }
        "Q18" => {
            cte_prelude(dir, &["customer", "orders", "lineitem"])
                + "SELECT c_name, c_custkey, o_orderkey, o_orderdate, o_totalprice, sum(l_quantity)
                 FROM customer, orders, lineitem
                 WHERE o_orderkey IN (
                         SELECT l_orderkey FROM lineitem GROUP BY l_orderkey
                         HAVING sum(l_quantity) > 300)
                   AND c_custkey = o_custkey AND o_orderkey = l_orderkey
                 GROUP BY c_name, c_custkey, o_orderkey, o_orderdate, o_totalprice
                 ORDER BY o_totalprice DESC, o_orderdate
                 LIMIT 100"
        }
        other => panic!("unknown query {other}"),
    }
}

/// Expected exact / bounded row count for a query (TPC-H invariants stable
/// across scale factors).
fn assert_rowcount_sane(name: &str, rs: &ResultSet) {
    let n = rs.row_count();
    assert!(n > 0, "{name} returned no rows: {rs:?}");
    match name {
        "Q1" => assert_eq!(n, 4, "Q1 must have 4 (returnflag,linestatus) groups"),
        "Q6" => assert_eq!(n, 1, "Q6 is a scalar revenue"),
        "Q5" => assert_eq!(n, 5, "Q5 has 5 ASIA nations"),
        "Q3" => assert!(n <= 10, "Q3 is LIMIT 10, got {n}"),
        "Q18" => assert!(n <= 100, "Q18 is LIMIT 100, got {n}"),
        _ => {}
    }
}

// --------------------------------------------------------------------------
// Percentiles
// --------------------------------------------------------------------------

fn pct(sorted_ms: &[f64], p: f64) -> f64 {
    if sorted_ms.is_empty() {
        return 0.0;
    }
    let idx = ((p / 100.0) * (sorted_ms.len() as f64 - 1.0)).round() as usize;
    sorted_ms[idx.min(sorted_ms.len() - 1)]
}

// ==========================================================================
// Scenario (a) — SF0.1 smoke: wiring + correctness on the REAL DuckDbEngine
// ==========================================================================

#[tokio::test]
#[ignore = "reads local TPC-H Parquet; run with --ignored"]
async fn tpch_smoke_sf01() {
    let root = data_root();
    let Some(dir) = sf_dir(&root, "sf0.1") else {
        eprintln!("SKIP tpch_smoke_sf01: {root}/sf0.1 not found");
        return;
    };
    let eng = engine_for(&root);
    println!(
        "\n==== TPC-H SMOKE (real DuckDbEngine, {}) @ sf0.1 ====",
        eng.version()
    );
    let l = lease(1024 * 1024 * 1024, 4);
    for name in ["Q1", "Q6", "Q3", "Q5", "Q18"] {
        let sql = tpch_sql(&dir, name);
        let t = Instant::now();
        let rs = eng
            .execute(&sql, l)
            .await
            .unwrap_or_else(|e| panic!("{name} failed: {e}"));
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        assert_rowcount_sane(name, &rs);
        // Determinism: a second run must hash identically.
        let rs2 = eng.execute(&sql, l).await.unwrap();
        assert_eq!(
            p2p_trust::canonical_hash(&rs),
            p2p_trust::canonical_hash(&rs2),
            "{name} not stable across runs"
        );
        println!(
            "  {name:<4} rows={:<4} {:>9.1} ms  (stable hash ok)",
            rs.row_count(),
            ms
        );
    }
    println!("==== smoke OK ====");
}

// ==========================================================================
// Scenario (b) — serial per-query baseline at SF1 & SF10
// ==========================================================================

#[tokio::test]
#[ignore = "reads multi-GB TPC-H Parquet; run with --ignored"]
async fn tpch_engine_serial() {
    let root = data_root();
    let eng = engine_for(&root);
    let threads: u32 = std::env::var("TPCH_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4);
    // Generous memory so the baseline is not spill-bound (spill is scenario d).
    let l = lease(8u64 * 1024 * 1024 * 1024, threads);

    println!(
        "\n==== TPC-H SERIAL BASELINE (real DuckDbEngine, {}) threads={threads} ====",
        eng.version()
    );
    println!(
        "{:<6} {:<5} {:>6} {:>12} {:>12}",
        "SF", "Q", "rows", "run1_ms", "run2_ms"
    );
    for sf in selected_sfs() {
        let Some(dir) = sf_dir(&root, &sf) else {
            eprintln!("SKIP {sf}: not found under {root}");
            continue;
        };
        for name in ["Q1", "Q6", "Q3", "Q5", "Q18"] {
            let sql = tpch_sql(&dir, name);
            let t1 = Instant::now();
            let rs = match eng.execute(&sql, l).await {
                Ok(r) => r,
                Err(e) => {
                    println!("{sf:<6} {name:<5} ERROR: {e}");
                    panic!("{sf} {name} failed: {e}");
                }
            };
            let ms1 = t1.elapsed().as_secs_f64() * 1000.0;
            assert_rowcount_sane(name, &rs);
            let t2 = Instant::now();
            let rs2 = eng.execute(&sql, l).await.unwrap();
            let ms2 = t2.elapsed().as_secs_f64() * 1000.0;
            assert_eq!(
                p2p_trust::canonical_hash(&rs),
                p2p_trust::canonical_hash(&rs2),
                "{sf} {name} unstable across runs"
            );
            println!(
                "{sf:<6} {name:<5} {:>6} {:>12.1} {:>12.1}",
                rs.row_count(),
                ms1,
                ms2
            );
            // Print scalar references for Q1/Q6 (stable correctness anchors).
            if name == "Q6" {
                println!("        Q6 revenue = {:?}", rs.rows[0][0]);
            }
            if name == "Q1" {
                println!("        Q1 first group = {:?}", rs.rows[0]);
            }
        }
    }
    println!("==== serial baseline done ====");
}

// ==========================================================================
// Scenario (c) — concurrency 8/16/32 through the AdmissionController + engine
// ==========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "concurrent big queries on real DuckDbEngine; run with --ignored"]
async fn tpch_concurrency() {
    use p2p_config::{BudgetConfig, DataClassCfg};
    use p2p_node::AdmissionController;

    let root = data_root();
    let sf = std::env::var("TPCH_CONC_SF").unwrap_or_else(|_| "sf1".to_string());
    let Some(dir) = sf_dir(&root, &sf) else {
        eprintln!("SKIP tpch_concurrency: {root}/{sf} not found");
        return;
    };
    let eng = Arc::new(engine_for(&root));

    // A real AdmissionController bounds concurrency: max_jobs slots + a memory
    // budget. Each query reserves per_job_memory before executing; over-capacity
    // offers are REJECTED (counted separately), exactly as a worker would.
    let per_job_mem: u64 = 1024 * 1024 * 1024; // 1 GiB per job
    let max_jobs: u32 = std::env::var("TPCH_MAX_JOBS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);
    let budget = BudgetConfig {
        memory_bytes: per_job_mem * max_jobs as u64,
        threads: max_jobs * 2,
        max_jobs,
        per_job_memory_bytes: per_job_mem,
        per_job_threads: 2,
        max_spill_bytes: 0,
        local_reserved_fraction: 0.0,
        data_classes: vec![DataClassCfg::Public],
    };
    let admission = AdmissionController::new(&budget);

    // Big-query mix (plan §3c): Q1/Q6/Q5/Q18.
    let mix = ["Q1", "Q6", "Q5", "Q18"];

    println!("\n==== TPC-H CONCURRENCY (real DuckDbEngine) @ {sf}  max_jobs={max_jobs} ====");
    println!(
        "{:>6} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>10}",
        "offered", "ok", "rejected", "errors", "p50_ms", "p95_ms", "max_ms", "thr_q/s"
    );

    for &offered in &[8usize, 16, 32] {
        let batch_start = Instant::now();
        let mut handles = Vec::with_capacity(offered);
        for i in 0..offered {
            let name = mix[i % mix.len()];
            let sql = tpch_sql(&dir, name);
            let eng = Arc::clone(&eng);
            let admission = Arc::clone(&admission);
            handles.push(tokio::spawn(async move {
                // Admission/lease accounting under load: reserve a slot+budget.
                let lease_guard = admission.try_admit(per_job_mem, 2);
                let Some(_guard) = lease_guard else {
                    return (name, None, true); // rejected at capacity
                };
                let t = Instant::now();
                let res = eng.execute(&sql, lease(per_job_mem, 2)).await;
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                // _guard drops here, releasing the lease back to the budget.
                match res {
                    Ok(rs) => {
                        assert_rowcount_sane(name, &rs);
                        (name, Some(ms), false)
                    }
                    Err(e) => {
                        eprintln!("  {name} exec error: {e}");
                        (name, None, false)
                    }
                }
            }));
        }

        let mut lats = Vec::new();
        let mut ok = 0usize;
        let mut rejected = 0usize;
        let mut errors = 0usize;
        for h in handles {
            let (_name, lat, was_rejected) = h.await.unwrap();
            if was_rejected {
                rejected += 1;
            } else if let Some(ms) = lat {
                ok += 1;
                lats.push(ms);
            } else {
                errors += 1;
            }
        }
        let wall = batch_start.elapsed().as_secs_f64().max(1e-9);
        lats.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let thr = ok as f64 / wall;
        println!(
            "{:>6} {:>9} {:>9} {:>9} {:>9.1} {:>9.1} {:>9.1} {:>10.2}",
            offered,
            ok,
            rejected,
            errors,
            pct(&lats, 50.0),
            pct(&lats, 95.0),
            lats.last().copied().unwrap_or(0.0),
            thr
        );
        assert_eq!(errors, 0, "no execution errors expected at {offered}-wide");
        assert!(
            ok + rejected == offered,
            "every offered query accounted for"
        );
    }
    println!("==== concurrency done ====");
}

// ==========================================================================
// Scenario (d) — larger-than-memory: SF10 Q1 & Q18 under a tight memory_limit
//                must complete via spill (out-of-core), not OOM.
// ==========================================================================

#[tokio::test]
#[ignore = "SF10 spill test; reads multi-GB Parquet; run with --ignored"]
async fn tpch_spill() {
    let root = data_root();
    let sf = std::env::var("TPCH_SPILL_SF").unwrap_or_else(|_| "sf10".to_string());
    let Some(dir) = sf_dir(&root, &sf) else {
        eprintln!("SKIP tpch_spill: {root}/{sf} not found");
        return;
    };
    let eng = engine_for(&root);

    println!("\n==== TPC-H LARGER-THAN-MEMORY SPILL (real DuckDbEngine) @ {sf} ====");
    println!(
        "{:<5} {:>10} {:>4} {:>8} {:>12} {:<10}",
        "Q", "mem", "thr", "rows", "ms", "status"
    );

    // 512MB then a tighter 256MB; both well below SF10 working sets. The engine
    // gives each job a private 0700 temp_directory, so DuckDB spills there.
    for &mb in &[512u64, 256] {
        let mem = mb * 1024 * 1024;
        for name in ["Q1", "Q18"] {
            let sql = tpch_sql(&dir, name);
            let t = Instant::now();
            let res = eng.execute(&sql, lease(mem, 2)).await;
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            match res {
                Ok(rs) => {
                    assert_rowcount_sane(name, &rs);
                    println!(
                        "{name:<5} {:>8}MB {:>4} {:>8} {:>12.1} {:<10}",
                        mb,
                        2,
                        rs.row_count(),
                        ms,
                        "SPILL-OK"
                    );
                }
                Err(e) => {
                    // An OOM here is a real out-of-core limitation: report it, do
                    // not paper over it.
                    println!(
                        "{name:<5} {:>8}MB {:>4} {:>8} {:>12.1} {:<10}\n      error: {e}",
                        mb, 2, "-", ms, "FAILED"
                    );
                    panic!("{name} @ {mb}MB did not complete (expected spill): {e}");
                }
            }
        }
    }
    println!("==== spill done ====");
}

// ==========================================================================
// Scenario (e) — grid path over loopback QUIC with prefer=>remote + quorum,
//                each worker backed by the REAL DuckDbEngine.
// ==========================================================================

mod grid {
    use super::*;
    use std::net::SocketAddr;

    use p2p_config::{GridConfig, IdentityConfig, PinningMode, QueryOverrides};
    use p2p_node::{
        AdmissionController, Candidate, Coordinator, StaticDiscovery, Worker, WorkerParams,
    };
    use p2p_proto::{Attestation, NodeId};
    use p2p_transport::{NodeIdentity, QuicTransport, Transport};
    use p2p_trust::{InMemoryTrustStore, TrustStore};

    fn idcfg() -> IdentityConfig {
        IdentityConfig {
            key_path: None,
            pinning_mode: PinningMode::Tofu,
            allowlist: vec![],
        }
    }

    struct WorkerHandle {
        node_id: NodeId,
        addr: SocketAddr,
        _transport: Arc<QuicTransport>,
        _task: tokio::task::JoinHandle<()>,
    }

    async fn spawn_worker(cfg: &GridConfig, engine: Arc<dyn QueryEngine>) -> WorkerHandle {
        let transport = Arc::new(
            QuicTransport::bind_tuned(
                &cfg.network,
                &cfg.transport.quic,
                &idcfg(),
                NodeIdentity::generate().unwrap(),
                p2p_transport::VersionInfo::default(),
            )
            .unwrap(),
        );
        let admission = AdmissionController::new(&cfg.budget);
        let params = WorkerParams::from_config(cfg);
        let node_id = transport.local_node_id().clone();
        let addr = transport.local_addr().unwrap();
        let worker = Worker::new(
            transport.clone(),
            engine,
            admission,
            Attestation::stub_l0(),
            params,
        );
        let task = worker.spawn();
        WorkerHandle {
            node_id,
            addr,
            _transport: transport,
            _task: task,
        }
    }

    async fn make_coordinator(cfg: GridConfig, workers: &[WorkerHandle]) -> Coordinator {
        let req = Arc::new(
            QuicTransport::bind_tuned(
                &cfg.network,
                &cfg.transport.quic,
                &idcfg(),
                NodeIdentity::generate().unwrap(),
                p2p_transport::VersionInfo::default(),
            )
            .unwrap(),
        );
        let candidates: Vec<Candidate> = workers
            .iter()
            .map(|w| Candidate::new(Some(w.node_id.clone()), w.addr))
            .collect();
        let disc = Arc::new(StaticDiscovery::new(
            candidates,
            cfg.discovery.candidate_sample_size,
        ));
        let st: Arc<dyn TrustStore> = Arc::new(InMemoryTrustStore::new(&cfg.trust, &cfg.limits));
        Coordinator::new(req, disc, st, Arc::new(cfg), "duckdb-grid")
    }

    fn grid_config(replicas: usize, quorum: usize) -> GridConfig {
        let mut c = GridConfig::default();
        c.scheduler.replicas = replicas;
        c.scheduler.quorum = quorum;
        c.scheduler.offer_timeout_ms = 5_000;
        c.scheduler.dispatch_timeout_ms = 120_000;
        c.scheduler.attempt_deadline_ms = 180_000;
        c.scheduler.progress_interval_ms = 2_000;
        c.trust.min_trust = 0.0;
        c.discovery.candidate_sample_size = 8;
        // Generous host budgets so big queries are admitted.
        c.budget.memory_bytes = 16u64 << 30;
        c.budget.threads = 16;
        c.budget.max_jobs = 8;
        c.budget.per_job_memory_bytes = 4u64 << 30;
        c.budget.per_job_threads = 4;
        // Allow the worker engine to read the local data dir.
        c.storage.allowed_local_paths = vec![super::data_root()];
        c.storage.enable_remote_access = false;
        // Big results: lift caps + give a long host job timeout.
        c.transport.result.max_result_bytes = 256 * 1024 * 1024;
        c.worker.job_timeout_ms = 180_000;
        c.transport.result.parallel_min_bytes = 4096;
        c
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 6)]
    #[ignore = "grid loopback with real DuckDbEngine; run with --ignored"]
    async fn tpch_grid_loopback() {
        let root = super::data_root();
        let sf = std::env::var("TPCH_GRID_SF").unwrap_or_else(|_| "sf1".to_string());
        let Some(dir) = sf_dir(&root, &sf) else {
            eprintln!("SKIP tpch_grid_loopback: {root}/{sf} not found");
            return;
        };

        // replicas=3, quorum=2: a small redundant quorum (plan §3b/§e).
        let cfg = grid_config(3, 2);
        let _ = cfg.validate();

        // Stand up 3 workers, each backed by its own real DuckDbEngine bound to
        // the local data dir (same-host multi-node: every worker reads the same
        // path, plan §1b row 2 / §3b).
        let mut workers = Vec::new();
        for _ in 0..3 {
            let eng = Arc::new(super::engine_for(&root)) as Arc<dyn QueryEngine>;
            workers.push(spawn_worker(&cfg, eng).await);
        }
        let coord = make_coordinator(cfg.clone(), &workers).await;

        println!(
            "\n==== TPC-H GRID LOOPBACK (real DuckDbEngine x3, replicas=3 quorum=2) @ {sf} ===="
        );

        // prefer=>'remote'. No local executor is wired on this coordinator, so
        // every query is dispatched to the grid regardless; we still stamp the
        // remote preference to mirror the documented call.
        let mut overrides = QueryOverrides::default();
        overrides.prefer = Some(p2p_config::PreferMode::Remote);

        for name in ["Q1", "Q6", "Q5"] {
            let sql = tpch_sql(&dir, name);
            let t = Instant::now();
            let outcome = coord
                .run_query(&sql, overrides.clone())
                .await
                .unwrap_or_else(|e| panic!("grid {name} failed: {e}"));
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            super::assert_rowcount_sane(name, &outcome.result);
            println!(
                "  {name:<4} rows={:<4} {:>9.1} ms  executed_locally={} verified={} agreement={}/{} winner={:?} participants={}",
                outcome.result.row_count(),
                ms,
                outcome.executed_locally,
                outcome.verified,
                outcome.agreement,
                outcome.quorum,
                outcome.winner.as_ref().map(|w| w.as_str().get(..8).unwrap_or("").to_string()),
                outcome.participants.len(),
            );
            assert!(
                !outcome.executed_locally,
                "must run on the grid, not locally"
            );
            assert!(outcome.verified, "quorum should verify identical results");
            assert!(
                outcome.agreement >= outcome.quorum,
                "agreement {} < quorum {}",
                outcome.agreement,
                outcome.quorum
            );
        }
        println!("==== grid loopback done ====");
    }
}
