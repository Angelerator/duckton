//! `p2p-node` — the coordinator core that ties transport + trust + engine into a
//! working node (architecture §3).
//!
//! A node is symmetric: it can act as a **requester** ([`Coordinator`]) and as a
//! **worker/host** ([`Worker`]) simultaneously. All operational values flow from
//! [`p2p_config::GridConfig`]; every major collaborator is pluggable behind a
//! trait ([`QueryEngine`], [`Discovery`], [`p2p_trust::TrustStore`]).
//!
//! Phase coverage:
//!  * Phase 0/1 — Offer/Bid/Dispatch, admission control, hedged racing with
//!    commit-first and loser RESET.
//!  * Phase 2 — canonical result hashing + quorum, signed receipts + reputation,
//!    canary auditing.

pub mod admission;
pub mod antiabuse;
pub mod canary;
pub mod compression;
pub mod coordinator;
pub mod datasource;
pub mod discovery;
#[cfg(feature = "duckdb-engine")]
pub mod duckdb_engine;
pub mod engine;
pub mod estimator;
#[cfg(feature = "discovery-libp2p")]
pub mod libp2p_discovery;
pub mod membership;
pub mod node;
pub mod planner;
pub mod result_stream;
pub mod sandbox;
pub mod signer;
pub mod storage;
pub mod worker;

pub use admission::{AdmissionController, FreeResources, Lease};
pub use antiabuse::{cost_gate_reason, Blocklist, RateLimiter};
pub use canary::CanaryAuditor;
pub use coordinator::{Coordinator, CoordinatorError, QueryOutcome};
pub use datasource::{
    default_provider, AzureProvider, CloudCredential, DataFormat, DataSourceError, GcsProvider,
    HttpsProvider, LocalFileProvider, ProviderOptions, ProviderRegistry, S3Provider,
    StorageProvider, StorageSetup, SEALED_TOKEN_PREFIX,
};
pub use discovery::{Candidate, CandidateFilter, Discovery, StaticDiscovery};
#[cfg(feature = "duckdb-engine")]
pub use duckdb_engine::DuckDbEngine;
pub use engine::{EngineError, ExecLease, JobContext, MockEngine, QueryEngine};
pub use estimator::{
    csv_metadata, delta_metadata, estimate_parquet, estimate_table_files, estimate_text,
    estimate_working_set, ndjson_metadata, parquet_metadata_from_resultset,
    parse_explain_cardinality, Cmp, ColumnChunkMeta, DataFileMeta, DeltaMetadata, EstimateError,
    EstimateParams, IcebergMetadata, ParquetMetadata, Predicate, Projection, QueryShape,
    RowGroupMeta, ScanEstimate, TableFilesMetadata, TextMetadata, WorkingSetEstimate,
};
#[cfg(feature = "discovery-libp2p")]
pub use libp2p_discovery::{
    evaluate_ad, AdOutcome, DiscoveryError, Libp2pDiscovery, Libp2pDiscoveryConfig, NatParams,
    RelayLimits,
};
pub use membership::MembershipTable;
pub use node::{Node, NodeError};
pub use planner::{
    is_resource_exhaustion, DefaultPlanner, LocalExecutor, LocalOrRemotePlanner, LocalReservation,
    PlanDecision, PlanReason, PlanRequest, Route,
};
pub use sandbox::{
    build as build_sandbox, effective_backend, EgressAllowList, EgressEndpoint, IosSandbox,
    JobBudget, JobGuard, NoopSandbox, ResourceLimits, Sandbox, SandboxError, SandboxSpec,
};
pub use storage::{
    sealed_credential, Enclave, EncryptedObjectStore, FakeAzureSasProvider, FakeGcsProvider,
    FakeStsS3Provider, KeyRelease, LocalFakeStorage, StorageCredentialProvider, StorageError,
};
pub use signer::IdentitySigner;
pub use worker::{Worker, WorkerParams};
