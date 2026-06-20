//! DuckDB-backed end-to-end scenarios (real engine, locked-down sandbox).
//!
//! Gated behind the `duckdb-engine` feature (bundles DuckDB). Run with:
//!   SDKROOT=$(xcrun --show-sdk-path) \
//!     cargo test -p p2p-node --features duckdb-engine --test scenarios_duckdb
//!
//! These exercise the full pipeline (QUIC mTLS → Offer/Bid/Dispatch → real
//! DuckDB execution → canonical hash → quorum → streamed result) with the real
//! engine, including the security sandbox blocking a malicious query.
#![cfg(feature = "duckdb-engine")]

use std::net::SocketAddr;
use std::sync::Arc;

use p2p_config::{GridConfig, IdentityConfig, PinningMode};
use p2p_node::{
    AdmissionController, Candidate, Coordinator, DuckDbEngine, ExecLease, QueryEngine,
    StaticDiscovery, Worker, WorkerParams,
};
use p2p_proto::{Attestation, NodeId};
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

async fn spawn_duckdb_worker() -> WorkerHandle {
    let net = GridConfig::default().network;
    let transport =
        Arc::new(QuicTransport::bind(&net, &idcfg(), NodeIdentity::generate().unwrap()).unwrap());
    let engine: Arc<dyn QueryEngine> = Arc::new(DuckDbEngine::new().unwrap());
    let admission = AdmissionController::new(&GridConfig::default().budget);
    let params = WorkerParams::from_config(&GridConfig::default());
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

fn cfg(replicas: usize, quorum: usize) -> GridConfig {
    let mut c = GridConfig::default();
    c.scheduler.replicas = replicas;
    c.scheduler.quorum = quorum;
    c.trust.min_trust = 0.0;
    c.scheduler.dispatch_timeout_ms = 15_000;
    c.validate().unwrap();
    c
}

async fn coordinator(workers: &[&WorkerHandle], c: GridConfig) -> Coordinator {
    let net = GridConfig::default().network;
    let req =
        Arc::new(QuicTransport::bind(&net, &idcfg(), NodeIdentity::generate().unwrap()).unwrap());
    let candidates: Vec<Candidate> = workers
        .iter()
        .map(|w| Candidate::new(Some(w.node_id.clone()), w.addr))
        .collect();
    let disc = Arc::new(StaticDiscovery::new(candidates, 64));
    let store: Arc<dyn TrustStore> = Arc::new(InMemoryTrustStore::new(
        &GridConfig::default().trust,
        &GridConfig::default().limits,
    ));
    Coordinator::new(req, disc, store, Arc::new(c), "duckdb")
}

#[tokio::test]
async fn duckdb_two_node_query_matches_local() {
    let w1 = spawn_duckdb_worker().await;
    let w2 = spawn_duckdb_worker().await;
    let coord = coordinator(&[&w1, &w2], cfg(2, 2)).await;

    let sql = "SELECT i, i*i AS sq FROM generate_series(1, 1000) t(i) ORDER BY i";
    let outcome = coord.run_query(sql, Default::default()).await.unwrap();

    let expected = DuckDbEngine::new()
        .unwrap()
        .execute(
            sql,
            ExecLease {
                memory_bytes: 256 << 20,
                threads: 1,
                max_spill_bytes: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(outcome.result.row_count(), 1000);
    assert_eq!(outcome.result, expected);
    assert!(outcome.verified);
}

#[tokio::test]
async fn duckdb_sandbox_blocks_malicious_query_end_to_end() {
    let w1 = spawn_duckdb_worker().await;
    let w2 = spawn_duckdb_worker().await;
    let coord = coordinator(&[&w1, &w2], cfg(2, 2)).await;

    // A malicious query attempting to read a local file is blocked by the
    // locked-down execution engine, so no worker commits a result and the job
    // fails (no bad data is returned).
    let malicious = "SELECT * FROM read_csv_auto('/etc/passwd')";
    let result = coord.run_query(malicious, Default::default()).await;
    assert!(
        result.is_err(),
        "malicious query must not yield a result: {result:?}"
    );
}
