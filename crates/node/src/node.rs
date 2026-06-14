//! Zero-config "just works" node façade (architecture §12 SQL surface, §17
//! config layering).
//!
//! [`Node`] is the *frictionless default* entrypoint a requester uses to run a
//! query with **effectively zero setup**: no prior `p2p_join` / `p2p_share`, no
//! config file, and no environment variables. It wires the transport, discovery,
//! trust store, the free in-process local-execution path and the local-vs-remote
//! planner into a ready [`Coordinator`] from nothing but the built-in
//! [`GridConfig::default`] layering.
//!
//! ## What auto-initializes (with which defaults)
//! * **Identity** — an ephemeral Ed25519 node key (or `identity.key_path` if set).
//! * **Transport** — a loopback QUIC endpoint on an OS-assigned port.
//! * **Discovery** — [`StaticDiscovery`] seeded from `discovery.bootstrap`
//!   (empty by default ⇒ local-first; populate it to fan out to a real grid).
//! * **Trust / scheduler / budget** — the documented defaults (replicas=3,
//!   quorum=2, `min_trust`, `min_attestation=L0`, `verify=quorum`, …).
//! * **Payment** — **free / no-chain** by default (`economics.enabled=false`),
//!   so there is no wallet/payment friction.
//! * **Planner** — `prefer = auto`: small queries run locally for free, big
//!   ones (or, when seeds are configured, anything routed remote) fan out to the
//!   grid. With **no reachable grid** the node gracefully runs **local-first**
//!   rather than failing.
//!
//! Everything stays customizable: the [`GridConfig`] layering (file → env) and
//! the per-call [`QueryOverrides`] (named `p2p_query` params) remain the way to
//! tweak any of the above — you only touch them when you *want* to.

use std::net::ToSocketAddrs;
use std::sync::Arc;

use p2p_config::{ConfigError, DataClassCfg, GridConfig, PreferMode, QueryOverrides};
use p2p_settlement::StakeRegistry;
use p2p_transport::{NodeIdentity, QuicTransport, TransportError};
use p2p_trust::InMemoryTrustStore;

use crate::coordinator::{Coordinator, CoordinatorError, QueryOutcome};
use crate::discovery::{Candidate, StaticDiscovery};
use crate::engine::QueryEngine;
use crate::planner::{DefaultPlanner, LocalExecutor, LocalOrRemotePlanner};

/// Errors from building or querying a [`Node`].
#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    #[error("config error: {0}")]
    Config(#[from] ConfigError),
    #[error("transport error: {0}")]
    Transport(#[from] TransportError),
    #[error("engine init error: {0}")]
    Engine(String),
    #[error(transparent)]
    Query(#[from] CoordinatorError),
    /// A query asked for paid execution (`payment => 'paid'`, with
    /// `[economics].enabled = true`) but this node has no wallet / settlement
    /// rail wired, so it cannot escrow or settle a payment.
    #[error(
        "paid execution was requested (payment => 'paid') but this node has no wallet/settlement \
         configured. Either run free — pass `payment => 'free'` to p2p_query (the grid is free by \
         default) — or configure the [economics] settlement rail + wallet and attach it before \
         querying."
    )]
    WalletRequired,
}

/// A ready-to-use requester node assembled from a [`GridConfig`].
///
/// Build one with [`Node::auto`] (defaults → config file → env) or
/// [`Node::with_config`] (explicit config), then call [`Node::query`]. The first
/// call needs no prior `p2p_join`/`p2p_share`.
pub struct Node {
    coordinator: Coordinator,
    config: Arc<GridConfig>,
    /// Whether any grid targets (bootstrap seeds) are configured. When false the
    /// node has nowhere to dispatch, so `auto` queries run local-first for free.
    has_grid_targets: bool,
    /// Whether a wallet / stake registry is wired (gates `payment => 'paid'`).
    has_wallet: bool,
}

impl Node {
    /// Zero-config constructor: load [`GridConfig`] (built-in defaults → config
    /// file via `P2P_CONFIG` → `P2P_*` env) and assemble a node around `engine`.
    ///
    /// With no config file and no env vars this is purely the built-in defaults,
    /// so a query "just works" local-first and free.
    pub fn auto(engine: Arc<dyn QueryEngine>) -> Result<Self, NodeError> {
        let cfg = GridConfig::load(None)?;
        Self::with_config(cfg, engine)
    }

    /// Build a node around an explicit, already-resolved [`GridConfig`].
    pub fn with_config(config: GridConfig, engine: Arc<dyn QueryEngine>) -> Result<Self, NodeError> {
        config.validate()?;

        // Identity: load a persisted key if configured, else a fresh ephemeral one.
        let identity = match &config.identity.key_path {
            Some(path) => NodeIdentity::from_pem_file(path)?,
            None => NodeIdentity::generate()?,
        };
        let transport = Arc::new(QuicTransport::bind(
            &config.network,
            &config.identity,
            identity,
        )?);

        // Discovery: turn configured bootstrap seeds into candidate workers.
        // Empty by default ⇒ local-first.
        let candidates = resolve_seeds(&config.discovery.bootstrap);
        let has_grid_targets = !candidates.is_empty();
        let discovery = Arc::new(StaticDiscovery::new(
            candidates,
            config.discovery.candidate_sample_size,
        ));

        let trust_store = Arc::new(InMemoryTrustStore::new(&config.trust, &config.limits));

        // Always wire the free in-process local-execution path + planner so the
        // node can run a query locally with no grid involvement.
        let engine_version = engine.version();
        let local = LocalExecutor::new(engine, config.budget.memory_bytes, &config.planner);
        let planner: Arc<dyn LocalOrRemotePlanner> =
            Arc::new(DefaultPlanner::new(config.planner.clone()));

        let config = Arc::new(config);
        let mut coordinator = Coordinator::new(
            transport,
            discovery,
            trust_store,
            Arc::clone(&config),
            engine_version,
        )
        .with_local_execution(local, planner);

        // Anti-abuse: wire the persisted local deny-list so SQL `p2p_block` /
        // auto-block actually exclude flagged candidates on this requester. An
        // empty/missing blocklist is a no-op, so default behavior is unchanged.
        if config.antiabuse.enabled {
            let blocklist =
                Arc::new(crate::antiabuse::Blocklist::with_store(p2p_config::BlocklistStore::open()));
            coordinator = coordinator.with_blocklist(blocklist);
        }

        Ok(Self {
            coordinator,
            config,
            has_grid_targets,
            has_wallet: false,
        })
    }

    /// Attach a wallet / stake registry, enabling `payment => 'paid'` execution
    /// (otherwise paid queries return [`NodeError::WalletRequired`]).
    pub fn with_wallet(mut self, registry: Arc<dyn StakeRegistry>) -> Self {
        self.coordinator = self.coordinator.with_stake_registry(registry);
        self.has_wallet = true;
        self
    }

    /// The resolved configuration backing this node.
    pub fn config(&self) -> &GridConfig {
        &self.config
    }

    /// The underlying coordinator (for advanced/grid-specific flows).
    pub fn coordinator(&self) -> &Coordinator {
        &self.coordinator
    }

    /// Run a query with the simplest possible contract.
    ///
    /// * No prior `p2p_join`/`p2p_share` is required.
    /// * With `prefer = auto` (the default) and no reachable grid, the query
    ///   runs **locally for free**; if a grid is configured it routes there and
    ///   falls back to local only if the grid is unreachable.
    /// * `overrides` are the per-call customization knobs (`replicas`, `quorum`,
    ///   `verify`, `prefer`, `payment`, `min_trust`, `min_attestation`, …).
    pub async fn query(
        &self,
        sql: &str,
        overrides: QueryOverrides,
    ) -> Result<QueryOutcome, NodeError> {
        let cfg = overrides.apply(&self.config)?;

        // Friendly guard: a paid job needs a wallet/settlement rail. Public data
        // under the default `auto` payment is free, so this only trips when the
        // caller (or config) actually resolves to PAID. Point at the override.
        if cfg
            .economics
            .resolve_payment(DataClassCfg::Public)
            .is_paid()
            && !self.has_wallet
        {
            return Err(NodeError::WalletRequired);
        }

        let effective_prefer = overrides.prefer.unwrap_or(cfg.planner.prefer);
        let mut ov = overrides;

        // Remote-only ("route everything to the grid") mode: when local execution
        // is disabled this node is a pure requester / thin-client and must NEVER
        // run a query on its own machine, so the local-first conveniences below
        // are suppressed. A query with no reachable hosts surfaces NoCandidates
        // cleanly instead of silently falling back to local.
        let local_allowed = cfg.planner.local_execution_enabled;

        // Zero-config local-first: in `auto` with no reachable grid there is
        // nowhere to dispatch, so run the query locally and free rather than
        // failing with NoCandidates.
        if local_allowed && matches!(effective_prefer, PreferMode::Auto) && !self.has_grid_targets {
            ov.prefer = Some(PreferMode::Local);
        }

        match self.coordinator.run_query(sql, ov.clone()).await {
            Ok(outcome) => Ok(outcome),
            // Graceful fallback: the user did not pin remote and the grid turned
            // out to be unreachable / insufficient → run locally for free. Not in
            // remote-only mode, where the NoCandidates error is surfaced as-is.
            Err(CoordinatorError::NoCandidates)
            | Err(CoordinatorError::InsufficientWorkers { .. })
                if local_allowed && matches!(effective_prefer, PreferMode::Auto) =>
            {
                ov.prefer = Some(PreferMode::Local);
                Ok(self.coordinator.run_query(sql, ov).await?)
            }
            Err(e) => Err(e.into()),
        }
    }
}

/// Parse configured bootstrap seeds (`quic://host:port`, `host:port`, or a bare
/// resolvable name) into discovery [`Candidate`]s. Unparseable entries are
/// skipped (they may be libp2p multiaddrs handled by the Kademlia overlay).
fn resolve_seeds(seeds: &[String]) -> Vec<Candidate> {
    let mut out = Vec::new();
    for seed in seeds {
        let host = seed
            .strip_prefix("quic://")
            .or_else(|| seed.strip_prefix("udp://"))
            .unwrap_or(seed)
            .trim_end_matches('/');
        // Direct socket-addr first (no DNS); then a best-effort resolve.
        if let Ok(addr) = host.parse() {
            out.push(Candidate::new(None, addr));
        } else if let Ok(mut addrs) = host.to_socket_addrs() {
            if let Some(addr) = addrs.next() {
                out.push(Candidate::new(None, addr));
            }
        }
    }
    out
}
