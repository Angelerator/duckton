//! Throughput + latency benchmark over in-process loopback QUIC (architecture
//! "Transport performance tuning"). Two in-process nodes (a requester
//! coordinator + a single worker) run on the deterministic mock engine so the
//! measurement isolates the transport/result path, not SQL execution.
//!
//! It reports:
//!   (a) rows/sec and MB/s for a large result transfer, and
//!   (b) round-trip latency for a small query.
//!
//! Parameters are configurable via env vars so the same harness scales from a
//! fast CI smoke run (the defaults) to a heavier local benchmark:
//!   P2P_BENCH_ROWS         (default 50000)   rows in the large result
//!   P2P_BENCH_PARALLELISM  (default 4)       concurrent result streams
//!   P2P_BENCH_COMPRESSION  (default none)    none|lz4|zstd
//!   P2P_BENCH_LAT_ITERS    (default 30)      small-query latency iterations
//!
//! Run with output:
//!   cargo test -p p2p-node --test benches -- --nocapture
//!
//! The assertions only check correctness (so CI stays deterministic); the
//! printed numbers are informational and naturally vary by machine.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use p2p_config::{CompressionAlgo, GridConfig, IdentityConfig, PinningMode, QueryOverrides};
use p2p_proto::{Attestation, NodeId, ResultSet, Value};
use p2p_node::{
    AdmissionController, Candidate, Coordinator, MockEngine, QueryEngine, StaticDiscovery, Worker,
    WorkerParams,
};
use p2p_transport::{NodeIdentity, QuicTransport, Transport};
use p2p_trust::{InMemoryTrustStore, TrustStore};

struct WorkerHandle {
    node_id: NodeId,
    addr: SocketAddr,
    _transport: Arc<QuicTransport>,
    _task: tokio::task::JoinHandle<()>,
}

fn idcfg() -> IdentityConfig {
    IdentityConfig {
        key_path: None,
        pinning_mode: PinningMode::Tofu,
        allowlist: vec![],
    }
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn env_compression() -> CompressionAlgo {
    match std::env::var("P2P_BENCH_COMPRESSION").as_deref() {
        Ok("lz4") => CompressionAlgo::Lz4,
        Ok("zstd") => CompressionAlgo::Zstd,
        _ => CompressionAlgo::None,
    }
}

fn env_congestion() -> p2p_config::CongestionAlgo {
    match std::env::var("P2P_BENCH_CONGESTION").as_deref() {
        Ok("bbr") => p2p_config::CongestionAlgo::Bbr,
        Ok("newreno") => p2p_config::CongestionAlgo::NewReno,
        _ => p2p_config::CongestionAlgo::Cubic,
    }
}

async fn spawn_worker(cfg: &GridConfig, engine: Arc<dyn QueryEngine>) -> WorkerHandle {
    // Bind the worker with the same QUIC tuning the benchmark exercises.
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
    let worker = Worker::new(transport.clone(), engine, admission, Attestation::stub_l0(), params);
    let task = worker.spawn();
    WorkerHandle { node_id, addr, _transport: transport, _task: task }
}

async fn make_coordinator(cfg: GridConfig, w: &WorkerHandle) -> Coordinator {
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
    let disc = Arc::new(StaticDiscovery::new(
        vec![Candidate::new(Some(w.node_id.clone()), w.addr)],
        cfg.discovery.candidate_sample_size,
    ));
    let st: Arc<dyn TrustStore> = Arc::new(InMemoryTrustStore::new(&cfg.trust, &cfg.limits));
    Coordinator::new(req, disc, st, Arc::new(cfg), "mock-1")
}

/// A bench-tuned base config: a single worker, generous budget + timeouts, trust
/// gate relaxed so the fresh worker is selectable.
fn bench_config() -> GridConfig {
    let mut c = GridConfig::default();
    c.scheduler.replicas = 1;
    c.scheduler.quorum = 1;
    c.scheduler.offer_timeout_ms = 5_000;
    c.scheduler.dispatch_timeout_ms = 60_000;
    c.trust.min_trust = 0.0;
    c.discovery.candidate_sample_size = 4;
    c.budget.memory_bytes = 64u64 << 30;
    c.budget.threads = 16;
    c.budget.max_jobs = 16;
    c.transport.result.parallel_min_bytes = 4096;
    c
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bench_throughput_and_latency_loopback() {
    let rows = env_usize("P2P_BENCH_ROWS", 50_000);
    let parallelism = env_usize("P2P_BENCH_PARALLELISM", 4);
    let lat_iters = env_usize("P2P_BENCH_LAT_ITERS", 30);
    let compression = env_compression();

    // ----- (a) throughput: one large result transfer -----
    let data: Vec<Vec<Value>> = (0..rows as i64)
        .map(|i| {
            vec![
                Value::Int(i),
                Value::Int(i.wrapping_mul(2_654_435_761)),
                Value::Text(format!("payload-row-{}-abcdefgh", i % 1000)),
            ]
        })
        .collect();
    let big = ResultSet::new(vec!["id".into(), "v".into(), "label".into()], data);
    let serialized_bytes = p2p_proto::to_bytes(&big).unwrap().len();

    let mut cfg = bench_config();
    cfg.transport.quic.congestion = env_congestion();
    cfg.transport.result.parallelism = parallelism;
    cfg.transport.compression.algorithm = compression;
    cfg.transport.compression.min_size_bytes = 4096;
    if (parallelism as u32) > cfg.transport.quic.max_concurrent_uni_streams {
        cfg.transport.quic.max_concurrent_uni_streams = parallelism as u32;
    }
    cfg.validate().unwrap();

    let mut fixtures = HashMap::new();
    fixtures.insert("BENCH_BIG".to_string(), big.clone());
    let worker = spawn_worker(&cfg, Arc::new(MockEngine::with_fixtures(fixtures))).await;
    let coord = make_coordinator(cfg.clone(), &worker).await;

    // Warm up one transfer (connection setup, code paths) before timing.
    let warm = coord.run_query("BENCH_BIG", QueryOverrides::default()).await.unwrap();
    assert_eq!(warm.result.row_count(), rows);

    let start = Instant::now();
    let outcome = coord.run_query("BENCH_BIG", QueryOverrides::default()).await.unwrap();
    let elapsed = start.elapsed();
    assert_eq!(outcome.result, big, "large result must reassemble exactly");

    let secs = elapsed.as_secs_f64().max(1e-9);
    let rows_per_sec = rows as f64 / secs;
    let mb = serialized_bytes as f64 / (1024.0 * 1024.0);
    let mb_per_sec = mb / secs;

    // ----- (b) latency: small query round trips -----
    let small = ResultSet::new(vec!["one".into()], vec![vec![Value::Int(1)]]);
    let mut sfix = HashMap::new();
    sfix.insert("BENCH_SMALL".to_string(), small.clone());
    let lat_worker = spawn_worker(&cfg, Arc::new(MockEngine::with_fixtures(sfix))).await;
    let lat_coord = make_coordinator(cfg.clone(), &lat_worker).await;
    // warm
    let _ = lat_coord.run_query("BENCH_SMALL", QueryOverrides::default()).await.unwrap();

    let mut samples = Vec::with_capacity(lat_iters);
    for _ in 0..lat_iters {
        let t = Instant::now();
        let o = lat_coord.run_query("BENCH_SMALL", QueryOverrides::default()).await.unwrap();
        assert_eq!(o.result.row_count(), 1);
        samples.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let avg = samples.iter().sum::<f64>() / samples.len() as f64;
    let p50 = samples[samples.len() / 2];
    let min = samples[0];
    let max = *samples.last().unwrap();

    println!("\n================ duckdb-p2p loopback transport benchmark ================");
    println!(
        "config: rows={rows} parallelism={parallelism} compression={compression:?} \
         congestion={:?} gso={}",
        cfg.transport.quic.congestion, cfg.transport.quic.gso
    );
    println!("(a) THROUGHPUT (one large result):");
    println!("    serialized result : {:.2} MB ({} bytes)", mb, serialized_bytes);
    println!("    transfer time     : {:.3} ms", elapsed.as_secs_f64() * 1000.0);
    println!("    rows/sec          : {:.0}", rows_per_sec);
    println!("    MB/sec            : {:.1}", mb_per_sec);
    println!("(b) LATENCY (small query, {lat_iters} iters):");
    println!("    min / p50 / avg / max ms : {:.3} / {:.3} / {:.3} / {:.3}", min, p50, avg, max);
    println!("=========================================================================\n");

    // Correctness-only assertions keep CI deterministic.
    assert!(rows_per_sec > 0.0);
    assert!(avg > 0.0);
}
