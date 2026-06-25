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

use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;

use p2p_config::{
    BlocklistStore, ConfigError, DataClassCfg, GridConfig, PreferMode, QueryOverrides,
};
use p2p_proto::{Attestation, NodeId};
use p2p_settlement::StakeRegistry;
use p2p_transport::{NodeIdentity, QuicTransport, Transport, TransportError};
use p2p_trust::InMemoryTrustStore;

use crate::admission::AdmissionController;
use crate::antiabuse::{Blocklist, RateLimiter};
use crate::coordinator::{Coordinator, CoordinatorError, QueryOutcome};
use crate::discovery::{Candidate, StaticDiscovery};
use crate::engine::QueryEngine;
use crate::estimator::{
    csv_metadata, delta_metadata, estimate_table_files, estimate_text, estimate_working_set,
    has_data_source, ndjson_metadata, EstimateParams, Projection, QueryShape, ScanEstimate,
    WorkingSetEstimate,
};
use crate::governor::CapacityGovernor;
use crate::planner::{DefaultPlanner, LocalExecutor, LocalOrRemotePlanner};
use crate::worker::{Worker, WorkerParams};

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
         default) — or configure          the [economics] settlement rail + wallet and attach it before \
         querying."
    )]
    WalletRequired,
    /// The libp2p discovery overlay (Kademlia DHT + gossip) failed to start.
    #[error("discovery error: {0}")]
    Discovery(String),
}

/// A ready-to-use requester node assembled from a [`GridConfig`].
///
/// Build one with [`Node::auto`] (defaults → config file → env) or
/// [`Node::with_config`] (explicit config), then call [`Node::query`]. The first
/// call needs no prior `p2p_join`/`p2p_share`.
pub struct Node {
    coordinator: Coordinator,
    config: Arc<GridConfig>,
    /// The query engine, retained so the node can also act as a host/worker
    /// ([`Node::spawn_host`]) using the same in-process engine that backs its
    /// free local-execution path.
    engine: Arc<dyn QueryEngine>,
    /// Whether any grid targets (bootstrap seeds) are configured. When false the
    /// node has nowhere to dispatch, so `auto` queries run local-first for free.
    has_grid_targets: bool,
    /// Whether a wallet / stake registry is wired (gates `payment => 'paid'`).
    has_wallet: bool,
    /// Process-wide capacity governor shared between this node's OWN local
    /// execution path (in the coordinator's `LocalExecutor`) and the host
    /// [`Worker`]'s `AdmissionController`. It is the single hard machine cap that
    /// stops own + served work from jointly oversubscribing RAM / threads / job
    /// slots when the node plays both roles.
    governor: Arc<CapacityGovernor>,
    /// The live libp2p discovery overlay (Kademlia DHT + gossip), present only
    /// after [`Node::enable_libp2p_discovery`]. Held for the node's lifetime so
    /// the swarm task stays alive, and so a hosting node can publish its signed
    /// capability ad to the swarm (see [`Node::spawn_host`]).
    #[cfg(feature = "discovery-libp2p")]
    libp2p: Option<Arc<crate::libp2p_discovery::Libp2pDiscovery>>,
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
    pub fn with_config(
        config: GridConfig,
        engine: Arc<dyn QueryEngine>,
    ) -> Result<Self, NodeError> {
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
        // Retain a handle to the engine so the node can also host (worker) with
        // the same engine; `LocalExecutor` takes its own clone for local exec.
        let engine_for_host = Arc::clone(&engine);

        // Process-wide capacity governor: the single hard machine cap shared by
        // the node's OWN local execution and the jobs it SERVES as a host, so the
        // two roles can never jointly oversubscribe RAM / threads / job slots.
        // `local_active` keys off whether this node will ever run its own
        // queries: a remote-only node (local execution disabled) gets the full
        // budget for serving, exactly as before (back-compat).
        let local_active = config.planner.enabled && config.planner.local_execution_enabled;
        let governor = CapacityGovernor::new(
            &config.budget,
            config.limits.worker_pool_size,
            config.budget.local_reserved_fraction,
            local_active,
        );

        let local = LocalExecutor::governed(
            engine,
            config.budget.memory_bytes,
            &config.planner,
            config.budget.per_job_threads,
            config.budget.per_job_memory_bytes,
            Arc::clone(&governor),
        );
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
            let blocklist = Arc::new(crate::antiabuse::Blocklist::with_store(
                p2p_config::BlocklistStore::open(),
            ));
            coordinator = coordinator.with_blocklist(blocklist);
        }

        // Per-job storage access (architecture §9.2): only when the operator opts
        // into remote object-store reads (`enable_remote_access`) do we wire an
        // access-granting provider — so a default node (local data, egress off)
        // attaches nothing, exactly as before. The mode selects HOW the worker is
        // granted read access:
        //  * presigned (default open grid): the requester signs per-object HTTPS
        //    URLs so commodity workers read with NO secret on the host.
        //  * scoped_secret / sealed: mint a short-lived scoped credential the
        //    worker turns into a `CREATE SECRET` (cloud issuers are the crate's
        //    shaped-but-fake providers; swap in real STS/SAS for production).
        if config.storage.enable_remote_access {
            match config.storage.credential_mode {
                p2p_config::CredentialMode::Presigned => {
                    if let Some(provider) =
                        crate::storage::default_presign_provider(&config.storage)
                    {
                        coordinator = coordinator.with_presign_provider(provider);
                    }
                }
                p2p_config::CredentialMode::ScopedSecret | p2p_config::CredentialMode::Sealed => {
                    if let Some(provider) =
                        crate::storage::default_credential_provider(&config.storage)
                    {
                        coordinator = coordinator.with_credential_provider(provider);
                    }
                }
            }
        }

        Ok(Self {
            coordinator,
            config,
            engine: engine_for_host,
            has_grid_targets,
            has_wallet: false,
            governor,
            #[cfg(feature = "discovery-libp2p")]
            libp2p: None,
        })
    }

    /// Spawn the **libp2p discovery overlay** (Kademlia DHT + gossipsub) and route
    /// candidate discovery through the live, self-advertised swarm membership
    /// instead of a static seed list. Listen/bootstrap/NAT all come from the
    /// resolved `[discovery]` config (bootstrap entries are libp2p multiaddrs in
    /// this mode). This is the production discovery path for an open swarm: a host
    /// node publishes its signed [`CapabilityAd`](p2p_proto::CapabilityAd) and any
    /// requester finds candidates from the gossiped, PoW-and-signature-verified
    /// [`MembershipTable`](crate::membership::MembershipTable).
    ///
    /// Call once, after construction. It is async because the overlay's swarm is
    /// started asynchronously. When this node also hosts ([`Node::spawn_host`]) it
    /// will periodically (re)publish its capability ad to the swarm so it stays in
    /// peers' membership views.
    #[cfg(feature = "discovery-libp2p")]
    pub async fn enable_libp2p_discovery(&mut self) -> Result<(), NodeError> {
        let disc = Arc::new(
            crate::libp2p_discovery::Libp2pDiscovery::from_config(&self.config)
                .await
                .map_err(|e| NodeError::Discovery(e.to_string()))?,
        );
        self.coordinator
            .set_discovery(Arc::clone(&disc) as Arc<dyn crate::discovery::Discovery>);
        // Bootstrapped into the swarm ⇒ this node can reach grid targets even
        // though no static QUIC seeds are configured.
        self.has_grid_targets = true;
        self.libp2p = Some(disc);
        Ok(())
    }

    /// Attach a wallet / stake registry, enabling `payment => 'paid'` execution
    /// (otherwise paid queries return [`NodeError::WalletRequired`]).
    pub fn with_wallet(mut self, registry: Arc<dyn StakeRegistry>) -> Self {
        self.coordinator = self.coordinator.with_stake_registry(registry);
        self.has_wallet = true;
        self
    }

    /// Attach the money rail + record anchor so PAID jobs open escrow, settle the
    /// payout split per the quorum verdict, and anchor the job record. FREE jobs
    /// (the default grid) are unaffected. Use the `ton` impls in production and
    /// the deterministic `mock` impls in tests.
    pub fn with_settlement(
        mut self,
        settlement: Arc<dyn p2p_settlement::Settlement>,
        anchor: Arc<dyn p2p_settlement::RecordAnchor>,
    ) -> Self {
        self.coordinator = self
            .coordinator
            .with_settlement(settlement)
            .with_record_anchor(anchor);
        self.has_wallet = true;
        self
    }

    /// Wire the on-chain `GlobalParams` read seam so PAID jobs follow the
    /// authoritative on-chain policy + version. Off by default (free/local nodes
    /// never read the chain). Pair with [`Node::spawn_params_sync`] to refresh it.
    pub fn with_params_source(mut self, source: Arc<dyn p2p_settlement::ParamsSource>) -> Self {
        self.coordinator = self.coordinator.with_params_source(source);
        self
    }

    /// Apply a resolved [`p2p_settlement::SettlementStack`] (the single
    /// construction site): wires the money rail + record anchor, and the optional
    /// on-chain `GlobalParams` read seam. Defaults safely to mock/noop per config,
    /// so a free/default node is unaffected.
    pub fn with_settlement_stack(mut self, stack: p2p_settlement::SettlementStack) -> Self {
        self.coordinator = self
            .coordinator
            .with_settlement(stack.settlement)
            .with_record_anchor(stack.record_anchor);
        if let Some(src) = stack.params_source {
            self.coordinator = self.coordinator.with_params_source(src);
        }
        // Wire the on-chain/in-memory stake registry so the `require_staked_hosts`
        // gate and the paid `stake_factor` reflect it. `None` (free grid) leaves
        // the coordinator's default behavior (no gate, `0.0` factor) untouched.
        if let Some(reg) = stack.stake_registry {
            self.coordinator = self.coordinator.with_stake_registry(reg);
        }
        if stack.onchain {
            self.has_wallet = true;
        }
        self
    }

    /// Start the startup + periodic on-chain `GlobalParams` sync (a no-op unless a
    /// params source is wired). Returns the task handle.
    pub fn spawn_params_sync(&self, interval: std::time::Duration) -> tokio::task::JoinHandle<()> {
        self.coordinator.spawn_params_sync(interval)
    }

    /// Inject the `NodeId → payout wallet` resolver (the node↔wallet binding
    /// lookup used to direct settlement payouts).
    pub fn with_wallet_resolver(
        mut self,
        resolver: Arc<dyn Fn(&NodeId) -> p2p_settlement::WalletAddress + Send + Sync>,
    ) -> Self {
        self.coordinator = self.coordinator.with_wallet_resolver(resolver);
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

    /// The live libp2p discovery overlay, present after
    /// [`Node::enable_libp2p_discovery`]. Exposes the overlay's bootstrap listen
    /// addresses ([`Libp2pDiscovery::listeners`](crate::libp2p_discovery::Libp2pDiscovery::listeners))
    /// — so a node can publish how peers should bootstrap to it — and the live,
    /// signature-and-PoW-verified swarm [`membership`](crate::libp2p_discovery::Libp2pDiscovery::membership)
    /// view, used to surface real network membership.
    #[cfg(feature = "discovery-libp2p")]
    pub fn libp2p_discovery(&self) -> Option<&Arc<crate::libp2p_discovery::Libp2pDiscovery>> {
        self.libp2p.as_ref()
    }

    /// This node's stable identity (its requester/worker node id).
    pub fn node_id(&self) -> &NodeId {
        self.coordinator.local_node_id()
    }

    /// The local QUIC socket address this node is bound to (host listen addr).
    pub fn local_addr(&self) -> Result<SocketAddr, TransportError> {
        self.coordinator.transport().local_addr()
    }

    /// Become a **host/worker**: spawn the worker accept loop on this node's
    /// live QUIC transport so it answers Offers/Dispatches from the grid using
    /// the same in-process engine. Returns the task handle — drop/abort it (or
    /// close the transport) to stop hosting.
    ///
    /// This is the engine behind the SQL `p2p_share` surface: a node that called
    /// it donates resources and executes queries for others. The advertised
    /// budget/attestation come from the resolved [`GridConfig`].
    pub fn spawn_host(&self) -> tokio::task::JoinHandle<()> {
        // Startup security-posture summary — the first thing to check when
        // troubleshooting "is my node accepting/rejecting/leaking as intended".
        // One structured line covering the controls an operator most often
        // misconfigures (identity pinning, OS sandbox, remote egress, on-chain).
        let sb = self.host_sandbox();
        tracing::info!(
            bind = %self.config.network.bind_addr,
            pinning = ?self.config.identity.pinning_mode,
            security_mode = ?self.config.security.mode,
            sandbox_enabled = sb.enabled,
            sandbox_backend = ?crate::sandbox::effective_backend(sb.backend),
            process_per_job = sb.process_per_job,
            remote_access = self.config.storage.enable_remote_access,
            egress_confined = self.host_remote_egress_confined(),
            economics = self.config.economics.enabled,
            "starting host: serving grid jobs (security posture summary)"
        );
        // Non-GDPR system-metadata capture (architecture: machine-class analytics
        // + routing HINT). Collect a signed `SystemProfile` on host start and
        // refresh it on the configured interval. A self-reported hint only — it
        // never feeds trust/selection scoring (kept out by construction).
        self.spawn_metadata_collection();
        // When the libp2p discovery overlay is active, advertise this host's
        // signed capability ad to the swarm so requesters can discover it live.
        #[cfg(feature = "discovery-libp2p")]
        self.spawn_capability_advertise();
        self.host_worker().spawn()
    }

    /// Periodically (re)publish this host's signed [`CapabilityAd`] to the libp2p
    /// gossip topic so it stays within peers' freshness window
    /// (`[discovery.gossip].capability_ttl_secs`). A no-op unless
    /// [`Node::enable_libp2p_discovery`] has run. The PoW is minted on a blocking
    /// thread (off the async runtime) and the ad's timestamp/PoW are refreshed
    /// each cycle so the ad never expires while the host serves. The task self-
    /// terminates once the discovery overlay is dropped (host stopped).
    #[cfg(feature = "discovery-libp2p")]
    fn spawn_capability_advertise(&self) {
        let Some(disc) = self.libp2p.as_ref() else {
            return;
        };
        let weak = Arc::downgrade(disc);
        let transport = self.coordinator.transport();
        let config = Arc::clone(&self.config);
        // The QUIC address peers dispatch jobs to: the operator-set advertised
        // address when present, else the bound socket address.
        let addr = config
            .network
            .advertised_addr
            .clone()
            .or_else(|| self.local_addr().ok().map(|a| a.to_string()));
        let Some(addr) = addr else {
            tracing::warn!(
                "libp2p discovery active but no advertised/bound QUIC address; \
                 skipping capability ad"
            );
            return;
        };
        let ttl = config.discovery.gossip.capability_ttl_secs;
        // Refresh well within the freshness window so the ad never goes stale.
        let readvertise = std::time::Duration::from_secs((ttl / 2).max(5));
        let bits = config.sybil.pow_difficulty_bits;
        tokio::spawn(async move {
            loop {
                // Stop advertising once the overlay (and thus the host) is gone.
                let Some(disc) = weak.upgrade() else {
                    break;
                };
                let t = Arc::clone(&transport);
                let c = Arc::clone(&config);
                let a = addr.clone();
                let ad = tokio::task::spawn_blocking(move || {
                    let id = t.identity();
                    let now = p2p_trust::now_ts();
                    let pow = p2p_trust::mint_pow(
                        &id.public_key_bytes(),
                        p2p_trust::sybil::pow_epoch(now),
                        bits,
                        POW_MINT_MAX_ITERS,
                    )?;
                    Some(build_host_capability_ad(&c, id, a, pow, now))
                })
                .await
                .ok()
                .flatten();
                match ad {
                    Some(ad) => {
                        if disc.publish_ad(&ad).await.is_err() {
                            break; // overlay task stopped
                        }
                    }
                    None => tracing::warn!(
                        pow_bits = bits,
                        "could not mint capability-ad PoW within iteration budget; \
                         will retry"
                    ),
                }
                // Release the strong ref BEFORE sleeping so the overlay's `Drop`
                // can fire (and this task exit on the next iteration) once the
                // node stops hosting.
                drop(disc);
                tokio::time::sleep(readvertise).await;
            }
        });
    }

    /// Collect + persist this host's signed [`p2p_proto::SystemProfile`] once on
    /// startup, then refresh it every `[metadata].refresh_interval_secs`. Detached
    /// best-effort task (collection/persist failures are logged and ignored).
    /// `refresh_interval_secs == 0` runs the one-shot startup collection only.
    fn spawn_metadata_collection(&self) {
        let interval_secs = self.config.metadata.refresh_interval_secs;
        let transport = self.coordinator.transport();
        let budget = self.config.budget.clone();
        let engine_version = self.engine.version();
        let extension_version = env!("CARGO_PKG_VERSION").to_string();
        tokio::spawn(async move {
            // The signer borrows the identity owned by the (Arc-held) transport,
            // which the task keeps alive for its whole lifetime.
            let signer = crate::signer::IdentitySigner(transport.identity());
            let store = crate::system_store::SystemStore::open();
            loop {
                let profile = crate::system_collect::collect_system_profile(
                    &signer,
                    &budget,
                    &engine_version,
                    &extension_version,
                );
                if let Err(e) = store.store(&signer, profile) {
                    tracing::debug!("system profile persist failed: {e}");
                }
                if interval_secs == 0 {
                    break; // one-shot startup collection only
                }
                tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;
            }
        });
    }

    /// Assemble the configured host [`Worker`] (without spawning it). Wires the
    /// worker-side anti-abuse runtime (deny-list / cost-gate / free-rate-limit,
    /// honoring `[antiabuse]`) and the OS-execution sandbox policy (`[sandbox]`),
    /// so the configured knobs actually take effect on the live host. A fresh
    /// zero-config node is unaffected: the deny-list is empty, the cost-gate and
    /// free-rate-limit default OFF, and the sandbox backend defaults to the noop
    /// (in-process) policy. Exposed (crate-internal) so the wiring is testable
    /// without standing up a QUIC accept loop.
    pub(crate) fn host_worker(&self) -> Worker {
        let transport = self.coordinator.transport();
        // Share the process-wide governor so SERVED jobs reserve from the same
        // hard cap as the node's OWN local execution (no dual-role
        // oversubscription).
        let admission =
            AdmissionController::governed(&self.config.budget, Arc::clone(&self.governor));
        let params = WorkerParams::from_config(&self.config);
        let mut worker = Worker::new(
            transport,
            self.host_engine(),
            admission,
            Attestation::stub_l0(),
            params,
        );
        if self.config.worker.self_measure_capability {
            worker = worker
                .with_capability_store(Arc::new(crate::capability_store::CapabilityStore::open()));
        }
        // Worker-side anti-abuse (ARCHITECTURE "Abuse resistance"): refuse blocked
        // requesters, pre-flight cost-gate, and free-job rate-limit — each gated by
        // its own `[antiabuse]` flag (mostly default-off). The deny-list is the
        // persisted one (so an operator's `p2p_block` now refuses the requester at
        // the host too), seeded empty on a fresh node ⇒ no behavior change.
        if self.config.antiabuse.enabled {
            let blocklist = Some(Arc::new(Blocklist::with_store(BlocklistStore::open())));
            let rate_limiter = if self.config.antiabuse.free_rate_limit_active() {
                let rl = &self.config.antiabuse.free_rate_limit;
                Some(Arc::new(RateLimiter::new(
                    rl.max_free_per_window,
                    rl.window_secs,
                    rl.max_tracked_requesters,
                )))
            } else {
                None
            };
            worker = worker.with_antiabuse(&self.config, blocklist, rate_limiter);
        }
        // OS-level execution sandbox policy (architecture §9.4). The HOST-serving
        // path is secure-by-default (`host_sandbox()` returns the OS sandbox +
        // process-per-job posture) while the requester's OWN self-run path is
        // never sandboxed. A configured `[sandbox]` backend + egress allow-list
        // applies on the live host; the hard, kill-the-job-not-the-node
        // enforcement is the process-per-job `SubprocessEngine` path wired in
        // `host_engine`.
        let host_sandbox = self.host_sandbox();
        worker = worker.with_sandbox(&host_sandbox, &self.config.storage);

        // Fail-safe remote-access guard (architecture §9.4, G8): a host that
        // opted into remote object-store reads (`storage.enable_remote_access`)
        // but has NO active OS egress filter would let a foreign query reach
        // ARBITRARY network endpoints (DuckDB's `enable_external_access` is
        // all-or-nothing; the in-process lockdown cannot scope egress). Rather
        // than silently allow open egress (an SSRF footgun) or crash, REFUSE to
        // serve: the host keeps running under the DuckDB lockdown but declines
        // every offer with a clear reason. Confinement requires the OS-sandboxed
        // process-per-job path with an egress-capable backend AND a configured
        // child executor; see `host_remote_egress_confined`.
        if self.config.storage.enable_remote_access && !self.host_remote_egress_confined() {
            let reason = "host refuses to serve: storage.enable_remote_access=true without an \
                 active OS egress filter (enable [sandbox] with process_per_job + an \
                 egress-capable backend and set P2P_JOB_EXEC, or run behind an OS-level egress \
                 firewall). Serving foreign jobs unconfined with open egress is an SSRF risk.";
            tracing::warn!("{reason}");
            worker = worker.refusing_to_serve(reason);
        }
        worker
    }

    /// The effective sandbox config for the HOST-serving path. The host is
    /// secure-by-default: when the operator left `[sandbox]` at the (neutral)
    /// global default, the host applies [`SandboxConfig::host_serving_secure`] (OS
    /// sandbox + process-per-job ON). Any explicit `[sandbox]` customization
    /// (e.g. `backend = "none"`, or tuned limits/egress) is honored verbatim, so
    /// an operator can still opt the host out. This NEVER affects the requester's
    /// own self-run path, which uses the global neutral default.
    fn host_sandbox(&self) -> p2p_config::SandboxConfig {
        if self.config.sandbox == p2p_config::SandboxConfig::default() {
            p2p_config::SandboxConfig::host_serving_secure()
        } else {
            self.config.sandbox.clone()
        }
    }

    /// Whether the host can confine a remote-access job's network EGRESS to the
    /// pinned storage endpoints (architecture §9.4). Egress is only actually
    /// filtered on the OS-sandboxed process-per-job path, so this requires: the
    /// sandbox enabled, `process_per_job` on, an egress-capable backend for this
    /// platform, AND a configured `P2P_JOB_EXEC` child executor (without it
    /// `host_engine` falls back to in-process, where egress is unfiltered).
    fn host_remote_egress_confined(&self) -> bool {
        use p2p_config::SandboxBackend::*;
        let sb = self.host_sandbox();
        if !sb.enabled || !sb.process_per_job {
            return false;
        }
        let egress_capable = matches!(
            crate::sandbox::effective_backend(sb.backend),
            MacosSeatbelt | CgroupsSeccomp | Android | WindowsJobObject
        );
        let have_executor = std::env::var("P2P_JOB_EXEC")
            .map(|p| !p.trim().is_empty())
            .unwrap_or(false);
        egress_capable && have_executor
    }

    /// The engine the host (worker) executes jobs with. Defaults to this node's
    /// in-process engine. When `[sandbox].process_per_job` is enabled AND a child
    /// executor is configured (`P2P_JOB_EXEC`), each job is instead run in an
    /// OS-sandboxed child process via [`crate::subprocess::SubprocessEngine`] (real
    /// G1/G8 enforcement). The flag-on-but-unconfigured case warns and keeps the
    /// in-process default, so the working path is never destabilized.
    fn host_engine(&self) -> Arc<dyn QueryEngine> {
        let sb = self.host_sandbox();
        if sb.process_per_job && sb.enabled {
            match std::env::var("P2P_JOB_EXEC") {
                Ok(path) if !path.trim().is_empty() => {
                    let sandbox = crate::sandbox::build(&sb);
                    return Arc::new(crate::subprocess::SubprocessEngine::new(
                        sandbox,
                        path,
                        Vec::new(),
                        format!("{}-subprocess", self.engine.version()),
                        sb.clone(),
                        self.config.storage.clone(),
                        self.config.worker.progress_interval_ms,
                    ));
                }
                _ => {
                    tracing::warn!(
                        "[sandbox].process_per_job is enabled but P2P_JOB_EXEC (the child \
                         executor path) is unset — falling back to the in-process engine. \
                         Build p2p-job-exec (--features duckdb-engine) and set P2P_JOB_EXEC \
                         to enable OS-enforced process-per-job execution."
                    );
                }
            }
        }
        Arc::clone(&self.engine)
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

        // Conservative pre-flight estimate (P1-2): only for confidently-tiny,
        // pure in-memory queries (no data source). It lets `auto` keep such a
        // query on the free local path even when a grid is configured, instead of
        // shipping it remote. `None` (any query with a data source, the common
        // case) ⇒ exactly today's routing. The full estimate source (a SQL-source
        // analyzer + engine Parquet/EXPLAIN probes) is deferred — see docs.
        let estimate = preflight_estimate(sql);

        match self
            .coordinator
            .run_query_planned(sql, ov.clone(), estimate)
            .await
        {
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

/// A cheap, metadata-only pre-flight working-set estimate (P1-2 / Part B).
///
/// Returns:
///  * a (tiny, zero-size) estimate for a query with NO data source (a pure
///    in-memory scalar like `SELECT 1 + 1`), so `auto` keeps it on the free local
///    path; and
///  * a **real, approximate** size estimate for a data-source query whose
///    sources we can size from local file/object metadata WITHOUT a scan
///    (Parquet/CSV/JSON file sizes, a Delta `_delta_log`); the estimate then
///    drives the requester's REMOTE size-based capability gate + worker bid
///    sizing (the locked-down local engine still cannot read external data, so a
///    data query is never routed local — see `Coordinator::run_query_planned`);
///    and
///  * `None` when the size is genuinely UNKNOWABLE pre-flight — a remote object
///    (`s3://…`) we cannot stat, a glob we cannot expand, a referenced table /
///    table function (`range(…)`, an attached DB) with no on-disk literal, or a
///    referenced local path that does not exist — preserving today's routing
///    (remote in `auto`) exactly.
///
/// HONEST scope: this is deliberately approximate. It does NOT parse projections,
/// predicates, or blocking-operator shape, so it assumes a full scan
/// (`Projection::All`, no pruning) and a streaming plan; Parquet size is
/// approximated from the on-disk file size × the columnar decompression ratio
/// (no footer is read without the engine). It therefore tends to OVER-estimate,
/// which is the safe direction for a capacity gate. The richer engine-backed
/// source (Parquet footer / `EXPLAIN` cardinality / operator shape) is deferred.
fn preflight_estimate(sql: &str) -> Option<WorkingSetEstimate> {
    // Pure in-memory query (no data source) ⇒ a confidently-tiny local estimate.
    if !has_data_source(sql) {
        return Some(WorkingSetEstimate {
            scanned_uncompressed_bytes: 0,
            estimated_rows: 0,
            scan_buffer_bytes: 0,
            group_by_bytes: 0,
            join_build_bytes: 0,
            sort_bytes: 0,
            peak_working_set_bytes: 0,
            estimated_runtime_ms: 0,
        });
    }
    // Data-source query: size it from local metadata if we can; otherwise `None`
    // (unknowable ⇒ route as today).
    size_data_sources(sql)
}

/// Maximum bytes sampled from a text (CSV/JSON) file to derive its average row
/// width (a bounded "HEAD"-style probe — never a full scan).
const PREFLIGHT_TEXT_SAMPLE_BYTES: usize = 64 * 1024;

/// Best-effort metadata-only size estimate for a data-source query, summing the
/// referenced LOCAL sources. Returns `None` (unknowable ⇒ route remote as today)
/// unless EVERY referenced data-source literal can be sized from local metadata.
fn size_data_sources(sql: &str) -> Option<WorkingSetEstimate> {
    let literals = extract_string_literals(sql);
    let params = EstimateParams::default();
    let mut total_scanned: u64 = 0;
    let mut total_rows: u64 = 0;
    let mut sized_sources = 0usize;

    for lit in literals {
        // A remote scheme (`s3://`, `https://`, …) cannot be stat-ed pre-flight,
        // and a glob cannot be expanded here ⇒ the whole estimate is unknowable.
        if lit.contains("://") {
            return None;
        }
        if lit.contains('*') || lit.contains('?') || lit.contains('[') {
            return None;
        }
        let path = std::path::Path::new(&lit);
        let looks_like_data = matches!(
            path.extension()
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Some("parquet" | "csv" | "tsv" | "json" | "ndjson" | "jsonl")
        );
        // Only treat a literal as a data source if it has a data extension or
        // actually exists on disk; an arbitrary string literal (e.g. a `WHERE`
        // value) is ignored.
        if !looks_like_data && !path.exists() {
            continue;
        }
        match scan_estimate_for_path(path, &params) {
            Some(est) => {
                total_scanned = total_scanned.saturating_add(est.scanned_uncompressed_bytes);
                total_rows = total_rows.saturating_add(est.total_rows);
                sized_sources += 1;
            }
            // A data-looking source we could not size (missing/unreadable) ⇒
            // unknowable; route as today rather than guessing.
            None => return None,
        }
    }

    // No on-disk source literal at all (e.g. `FROM mytable`, `FROM range(100)`):
    // the size is unknowable pre-flight ⇒ route remote, exactly as today.
    if sized_sources == 0 {
        return None;
    }

    let avg_row_width_bytes = if total_rows > 0 {
        total_scanned / total_rows
    } else {
        0
    };
    let scan = ScanEstimate {
        scanned_uncompressed_bytes: total_scanned,
        total_rows,
        estimated_output_rows: total_rows,
        avg_row_width_bytes,
        units_total: sized_sources,
        units_scanned: sized_sources,
        projected_columns: 0,
    };
    // No plan ⇒ assume a streaming scan (we cannot see blocking operators here);
    // the peak is the bounded scan buffer. This intentionally under-states peak
    // RAM but states the SCANNED bytes accurately — the proven-`max_input_bytes`
    // half of the capability gate keys on scanned bytes, which is what we know.
    Some(estimate_working_set(
        &scan,
        &QueryShape::streaming(),
        &params,
    ))
}

/// Metadata-only [`ScanEstimate`] for one local path. Delta directory →
/// `_delta_log`; CSV/TSV/JSON/NDJSON → bounded text sample; Parquet → file size ×
/// decompression ratio (no engine footer); any other existing file → raw size.
/// `None` when the path can't be sized (missing / unreadable).
fn scan_estimate_for_path(path: &std::path::Path, params: &EstimateParams) -> Option<ScanEstimate> {
    // Delta table directory (a `_delta_log/` of JSON commits — pure-Rust read).
    if path.is_dir() {
        if path.join("_delta_log").is_dir() {
            let meta = delta_metadata(path).ok()?;
            return Some(estimate_table_files(&meta, &Projection::All, &[], params));
        }
        return None;
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("csv") => {
            let meta = csv_metadata(path, b',', PREFLIGHT_TEXT_SAMPLE_BYTES).ok()?;
            Some(estimate_text(&meta, &Projection::All, &[], params))
        }
        Some("tsv") => {
            let meta = csv_metadata(path, b'\t', PREFLIGHT_TEXT_SAMPLE_BYTES).ok()?;
            Some(estimate_text(&meta, &Projection::All, &[], params))
        }
        Some("json" | "ndjson" | "jsonl") => {
            let meta = ndjson_metadata(path, PREFLIGHT_TEXT_SAMPLE_BYTES).ok()?;
            Some(estimate_text(&meta, &Projection::All, &[], params))
        }
        Some("parquet") => {
            // No pure-Rust footer reader here (engine-gated): approximate the
            // uncompressed scanned bytes from the on-disk size × the columnar
            // decompression ratio. Over-estimates (safe for a capacity gate).
            let size = std::fs::metadata(path).ok()?.len();
            let scanned = ((size as f64) * params.columnar_decompression_ratio).round() as u64;
            Some(raw_scan_estimate(scanned))
        }
        // Any other existing file: use its raw size as the scanned footprint.
        _ => {
            let size = std::fs::metadata(path).ok()?.len();
            Some(raw_scan_estimate(size))
        }
    }
}

/// A [`ScanEstimate`] for a single opaque source of `scanned_bytes` whose row
/// count is unknown (one scanned unit, no per-row detail).
fn raw_scan_estimate(scanned_bytes: u64) -> ScanEstimate {
    ScanEstimate {
        scanned_uncompressed_bytes: scanned_bytes,
        total_rows: 0,
        estimated_output_rows: 0,
        avg_row_width_bytes: 0,
        units_total: 1,
        units_scanned: 1,
        projected_columns: 0,
    }
}

/// Extract single-quoted string literals from SQL (DuckDB file/object literals
/// are single-quoted; `''` is an escaped quote). Comment/dollar-quote naive —
/// good enough for the conservative pre-flight router, which only ACTS on a
/// literal that also looks like / resolves to a local data source.
fn extract_string_literals(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            let mut lit = String::new();
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\'' {
                    // `''` escapes a single quote inside the literal.
                    if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                        lit.push('\'');
                        i += 2;
                        continue;
                    }
                    break;
                }
                lit.push(bytes[i] as char);
                i += 1;
            }
            out.push(lit);
        }
        i += 1;
    }
    out
}

/// Parse configured bootstrap seeds (`quic://host:port`, `host:port`, or a bare
/// resolvable name) into discovery [`Candidate`]s. Unparseable entries are
/// skipped (they may be libp2p multiaddrs handled by the Kademlia overlay).
/// PoW minting iteration budget for a host capability ad. The default difficulty
/// is 16 leading-zero bits (≈2¹⁶ hashes on average), so this is a wide safety
/// margin while still bounding worst-case CPU; minting runs on a blocking thread.
#[cfg(feature = "discovery-libp2p")]
const POW_MINT_MAX_ITERS: u64 = 50_000_000;

/// Build + sign this host's [`CapabilityAd`](p2p_proto::CapabilityAd) from the
/// resolved config and node identity, with a freshly minted PoW for the ad's
/// epoch. The advertised attributes mirror what the host actually enforces: the
/// donated budget (memory/threads/max jobs), the membership labels
/// (networks/groups/region), the standby flag (`[worker].enabled`), and the
/// price (0 on the free grid, else the metered per-second rate).
#[cfg(feature = "discovery-libp2p")]
fn build_host_capability_ad(
    config: &GridConfig,
    identity: &NodeIdentity,
    addr: String,
    pow: p2p_trust::sybil::PowStamp,
    ts: u64,
) -> p2p_proto::CapabilityAd {
    use p2p_proto::AttestationLevel;
    use p2p_trust::{sign_capability_ad, CapabilityDraft};

    let signer = crate::signer::IdentitySigner(identity);
    let price = if config.economics.enabled {
        config
            .economics
            .pricing
            .effective_rate_per_second()
            .min(u64::MAX as u128) as u64
    } else {
        0
    };
    sign_capability_ad(
        CapabilityDraft {
            addr,
            free_mem_bytes: config.budget.memory_bytes,
            free_threads: config.budget.threads,
            max_jobs: config.budget.max_jobs,
            // The host worker attests L0 by default (see `Attestation::stub_l0`
            // in `host_worker`); a `> L0` claim is only honored with verified
            // evidence at the coordinator, so advertise the honest baseline.
            attestation_level: AttestationLevel::L0,
            price,
            recent_receipts_root: None,
            pow,
            ts,
            enabled: config.worker.enabled,
            networks: config.membership.networks.clone(),
            groups: config.membership.groups.clone(),
            region: config.membership.region.clone(),
        },
        &signer,
    )
}

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

#[cfg(test)]
mod tests {
    use super::{preflight_estimate, Node};
    use crate::engine::MockEngine;
    use crate::planner::{DefaultPlanner, LocalOrRemotePlanner, PlanRequest, Route};
    use p2p_config::{GridConfig, PlannerConfig, PreferMode};
    use p2p_proto::{BidDecision, DataClass, JobId, NodeId, Offer, QueryHash};
    use std::io::Write;
    use std::sync::Arc;

    // ----- Pre-flight estimate (Part B.1) -----

    #[test]
    fn preflight_pure_in_memory_query_is_tiny_local_estimate() {
        // No data source ⇒ a confidently-tiny (zero-size) estimate, so `auto`
        // keeps the query on the free local path.
        let est = preflight_estimate("SELECT 1 + 1").expect("pure query gets an estimate");
        assert_eq!(est.scanned_uncompressed_bytes, 0);
        assert_eq!(est.peak_working_set_bytes, 0);
    }

    #[test]
    fn preflight_unknowable_sources_yield_none() {
        // A remote object we cannot stat pre-flight ⇒ None (route remote as today).
        assert!(
            preflight_estimate("SELECT * FROM read_parquet('s3://bucket/k.parquet')").is_none()
        );
        // A glob we cannot expand here ⇒ None.
        assert!(preflight_estimate("SELECT * FROM read_csv_auto('/data/*.csv')").is_none());
        // A relation / table function with no on-disk literal ⇒ None.
        assert!(preflight_estimate("SELECT * FROM range(100)").is_none());
        assert!(preflight_estimate("SELECT * FROM my_table").is_none());
        // A data-looking local literal that does not exist ⇒ None (unknowable).
        assert!(preflight_estimate("SELECT * FROM read_csv_auto('/nope/missing.csv')").is_none());
    }

    #[test]
    fn preflight_sizes_a_real_local_csv_by_metadata() {
        // A real local CSV is sized from its on-disk metadata (no scan / no engine):
        // the estimate is non-trivial and scales with the file.
        let dir = tempfile::tempdir().unwrap();
        let small = dir.path().join("small.csv");
        let big = dir.path().join("big.csv");
        {
            let mut f = std::fs::File::create(&small).unwrap();
            writeln!(f, "id,name").unwrap();
            for i in 0..50 {
                writeln!(f, "{i},row-{i}").unwrap();
            }
        }
        {
            let mut f = std::fs::File::create(&big).unwrap();
            writeln!(f, "id,name").unwrap();
            for i in 0..200_000 {
                writeln!(f, "{i},a-much-longer-row-value-{i}").unwrap();
            }
        }
        let q = |p: &std::path::Path| format!("SELECT * FROM read_csv_auto('{}')", p.display());
        let small_est = preflight_estimate(&q(&small)).expect("local csv is sizeable");
        let big_est = preflight_estimate(&q(&big)).expect("local csv is sizeable");
        assert!(small_est.scanned_uncompressed_bytes > 0);
        assert!(
            big_est.scanned_uncompressed_bytes > small_est.scanned_uncompressed_bytes,
            "the bigger file must estimate a larger scan"
        );
    }

    #[test]
    fn estimate_drives_local_vs_remote_routing_by_size() {
        // The planner routes by the pre-flight estimate's real size: a SMALL one
        // fits locally; a LARGE one (scanned bytes over the threshold) goes remote.
        let mut pc = PlannerConfig::default();
        pc.enabled = true;
        pc.local_execution_enabled = true;
        pc.prefer = PreferMode::Auto;
        pc.size_threshold_bytes = 64 * 1024 * 1024; // 64 MiB local cap
        let planner = DefaultPlanner::new(pc);

        let small = super::WorkingSetEstimate {
            scanned_uncompressed_bytes: 1_000,
            estimated_rows: 100,
            scan_buffer_bytes: 1_000,
            group_by_bytes: 0,
            join_build_bytes: 0,
            sort_bytes: 0,
            peak_working_set_bytes: 1_000,
            estimated_runtime_ms: 1,
        };
        let large = super::WorkingSetEstimate {
            scanned_uncompressed_bytes: 4 * 1024 * 1024 * 1024,
            peak_working_set_bytes: 4 * 1024 * 1024 * 1024,
            ..small.clone()
        };
        let req = |est| PlanRequest {
            prefer: PreferMode::Auto,
            estimate: Some(est),
            headroom_bytes: 8 * 1024 * 1024 * 1024,
            local_slot_available: true,
        };
        assert_eq!(planner.decide(&req(small)).route, Route::Local);
        assert_eq!(planner.decide(&req(large)).route, Route::Remote);
    }

    #[tokio::test]
    async fn host_worker_wires_cost_gate_from_config() {
        // The cost-gate is wired into the live host worker (the bug was that
        // `spawn_host` never called `with_antiabuse`). With it configured, an
        // over-budget offer is refused; a within-budget one is admitted — proving
        // the wiring, not a blanket refusal.
        let mut cfg = GridConfig::default();
        cfg.antiabuse.cost_gate.enabled = true;
        cfg.antiabuse.cost_gate.max_cost_hint_rows = 10;
        cfg.validate().unwrap();
        let node = Node::with_config(cfg, Arc::new(MockEngine::deterministic())).unwrap();
        let worker = node.host_worker();

        let mk = |rows: u64| Offer {
            job_id: JobId::new(),
            requester_id: NodeId("b3:req".into()),
            query_hash: QueryHash::compute("SELECT 1", "v"),
            cost_hint_rows: Some(rows),
            cost_hint_bytes: None,
            data_class: DataClass::Public,
            nonce: 0,
            network: None,
            groups: Vec::new(),
            regions: Vec::new(),
            group_proof: None,
            input_fingerprint_hint: None,
        };
        assert!(
            matches!(
                worker.bid_for(&mk(1_000)).decision,
                BidDecision::Reject { .. }
            ),
            "over-budget offer must be cost-gated"
        );
        assert!(
            matches!(worker.bid_for(&mk(5)).decision, BidDecision::Accept),
            "within-budget offer must still be admitted"
        );
    }

    #[tokio::test]
    async fn host_worker_enforces_request_scoping_admission() {
        // The worker is the always-correct enforcement point (§7.5): standby,
        // network, group and region constraints are checked in `make_bid` BEFORE
        // any acceptance. A zero-config host (default network, ungrouped, no
        // region) is unaffected unless the offer carries a constraint it can't
        // satisfy. Mirrors the `data_class`/`require_staked_hosts` admission tests.
        let mk = |network: Option<&str>, groups: Vec<&str>, regions: Vec<&str>| Offer {
            job_id: JobId::new(),
            requester_id: NodeId("b3:req".into()),
            query_hash: QueryHash::compute("SELECT 1", "v"),
            cost_hint_rows: Some(1),
            cost_hint_bytes: None,
            data_class: DataClass::Public,
            nonce: 0,
            network: network.map(String::from),
            groups: groups.into_iter().map(String::from).collect(),
            regions: regions.into_iter().map(String::from).collect(),
            group_proof: None,
            input_fingerprint_hint: None,
        };
        let build = |f: &dyn Fn(&mut GridConfig)| {
            let mut cfg = GridConfig::default();
            f(&mut cfg);
            cfg.validate().unwrap();
            Node::with_config(cfg, Arc::new(MockEngine::deterministic())).unwrap()
        };
        let accept = |b: &p2p_proto::Bid| matches!(b.decision, BidDecision::Accept);
        let reject = |b: &p2p_proto::Bid| matches!(b.decision, BidDecision::Reject { .. });

        // Zero-config host serves an unconstrained offer (no behavior change).
        let n = build(&|_| {});
        assert!(accept(&n.host_worker().bid_for(&mk(None, vec![], vec![]))));

        // Standby (enabled=false): decline EVERY new offer (graceful drain — the
        // gate is only in `make_bid`, so an executing lease is never interrupted).
        let n = build(&|c| c.worker.enabled = false);
        assert!(reject(&n.host_worker().bid_for(&mk(None, vec![], vec![]))));

        // Network: a host on ["eu"] rejects a "us"-targeted offer, serves "eu",
        // and (no network constraint) serves an untargeted offer.
        let n = build(&|c| c.membership.networks = vec!["eu".into()]);
        let w = n.host_worker();
        assert!(reject(&w.bid_for(&mk(Some("us"), vec![], vec![]))));
        assert!(accept(&w.bid_for(&mk(Some("eu"), vec![], vec![]))));
        assert!(accept(&w.bid_for(&mk(None, vec![], vec![]))));

        // Groups: a grouped host serves only requesters sharing a group; a
        // requester claiming none is rejected.
        let n = build(&|c| c.membership.groups = vec!["finance".into()]);
        let w = n.host_worker();
        assert!(reject(&w.bid_for(&mk(None, vec!["ops"], vec![]))));
        assert!(accept(&w.bid_for(&mk(None, vec!["finance"], vec![]))));
        assert!(reject(&w.bid_for(&mk(None, vec![], vec![]))));

        // Region: a host in "eu" rejects a ["us"] pin and serves an ["eu"] pin.
        let n = build(&|c| c.membership.region = Some("eu".into()));
        let w = n.host_worker();
        assert!(reject(&w.bid_for(&mk(None, vec![], vec!["us"]))));
        assert!(accept(&w.bid_for(&mk(None, vec![], vec!["eu"]))));
        // A host with NO declared region fails closed under a region pin.
        let n = build(&|_| {});
        assert!(reject(&n.host_worker().bid_for(&mk(
            None,
            vec![],
            vec!["eu"]
        ))));
    }

    #[tokio::test]
    async fn host_worker_token_group_tier_verifies_proof() {
        // Under `group_enforcement = token` a grouped host ignores the requester's
        // SOFT declared groups and instead requires a cryptographic `group_proof`
        // that (a) verifies against the configured issuer for one of the host's
        // groups and (b) is bound to the requester's identity. Mirrors the
        // soft-tier admission test but exercises the token path.
        use ed25519_dalek::SigningKey;
        use p2p_trust::{CapabilityToken, Caveat};
        use rand::rngs::OsRng;

        let issuer = SigningKey::generate(&mut OsRng);
        let issuer_hex = hex::encode(issuer.verifying_key().to_bytes());
        let requester = SigningKey::generate(&mut OsRng);
        let req_pub = requester.verifying_key().to_bytes();
        let req_id = NodeId::from_pubkey(&req_pub);
        let now = p2p_trust::now_ts();
        let mint = |group: &str, exp: u64| {
            let t = CapabilityToken::mint(
                &issuer,
                &req_pub,
                vec![Caveat::Group(group.into()), Caveat::ExpiresAt(exp)],
            );
            serde_json::to_string(&t).unwrap()
        };

        let mut cfg = GridConfig::default();
        cfg.membership.groups = vec!["finance".into()];
        cfg.membership.group_enforcement = p2p_config::GroupEnforcement::Token;
        cfg.membership
            .group_issuers
            .insert("finance".into(), issuer_hex);
        cfg.validate().unwrap();
        let node = Node::with_config(cfg, Arc::new(MockEngine::deterministic())).unwrap();
        let w = node.host_worker();

        let offer = |proof: Option<String>, declared: Vec<&str>| Offer {
            job_id: JobId::new(),
            requester_id: req_id.clone(),
            query_hash: QueryHash::compute("SELECT 1", "v"),
            cost_hint_rows: Some(1),
            cost_hint_bytes: None,
            data_class: DataClass::Public,
            nonce: 0,
            network: None,
            groups: declared.into_iter().map(String::from).collect(),
            regions: Vec::new(),
            group_proof: proof,
            input_fingerprint_hint: None,
        };
        let reject = |b: &p2p_proto::Bid| matches!(b.decision, BidDecision::Reject { .. });

        // Valid finance proof ⇒ accept (soft declared groups are irrelevant here).
        assert!(matches!(
            w.bid_for(&offer(Some(mint("finance", now + 100_000)), vec![]))
                .decision,
            BidDecision::Accept
        ));
        // A merely-declared group with no token ⇒ reject (soft claims don't count).
        assert!(reject(&w.bid_for(&offer(None, vec!["finance"]))));
        // A token for a different group ⇒ reject.
        assert!(reject(
            &w.bid_for(&offer(Some(mint("ops", now + 100_000)), vec![]))
        ));
        // An expired token ⇒ reject.
        assert!(reject(&w.bid_for(&offer(
            Some(mint("finance", now.saturating_sub(10))),
            vec![]
        ))));
        // A valid token NOT bound to this requester id ⇒ reject.
        let other = SigningKey::generate(&mut OsRng);
        let stolen = CapabilityToken::mint(
            &issuer,
            &other.verifying_key().to_bytes(),
            vec![
                Caveat::Group("finance".into()),
                Caveat::ExpiresAt(now + 100_000),
            ],
        );
        assert!(reject(&w.bid_for(&offer(
            Some(serde_json::to_string(&stolen).unwrap()),
            vec![]
        ))));
    }

    #[tokio::test]
    async fn host_worker_binds_offer_requester_to_authenticated_peer() {
        // P0 (always-on, public + private): the offer's self-claimed
        // `requester_id` must equal the authenticated mTLS peer. A spoofed offer
        // (requester_id != peer) is rejected; the honest case (they match, as in
        // every real dial) is admitted on the default public config.
        let node = Node::with_config(GridConfig::default(), Arc::new(MockEngine::deterministic()))
            .unwrap();
        let w = node.host_worker();
        let mk = |claimed: &str| Offer {
            job_id: JobId::new(),
            requester_id: NodeId(claimed.into()),
            query_hash: QueryHash::compute("SELECT 1", "v"),
            cost_hint_rows: Some(1),
            cost_hint_bytes: None,
            data_class: DataClass::Public,
            nonce: 0,
            network: None,
            groups: Vec::new(),
            regions: Vec::new(),
            group_proof: None,
            input_fingerprint_hint: None,
        };
        let peer = NodeId("b3:real-peer".into());

        // Honest flow: requester_id == authenticated peer ⇒ admitted.
        assert!(matches!(
            w.bid_for_peer(&peer, &mk("b3:real-peer")).decision,
            BidDecision::Accept
        ));
        // Impersonation: an offer claiming a DIFFERENT requester_id than the TLS
        // peer is rejected before any admission/group check.
        assert!(matches!(
            w.bid_for_peer(&peer, &mk("b3:someone-else")).decision,
            BidDecision::Reject { .. }
        ));
    }

    #[tokio::test]
    async fn private_mode_serves_only_rostered_grouped_tokened_requester() {
        // End-to-end private/enterprise closure on the host (worker) side: a
        // properly-tokened company node ON the roster succeeds, while an ungrouped
        // host, an unrostered requester, and a missing group token are all
        // rejected. The always-on peer binding is exercised throughout (peer ==
        // requester_id).
        use ed25519_dalek::SigningKey;
        use p2p_trust::{CapabilityToken, Caveat};
        use rand::rngs::OsRng;

        let issuer = SigningKey::generate(&mut OsRng);
        let issuer_hex = hex::encode(issuer.verifying_key().to_bytes());
        let requester = SigningKey::generate(&mut OsRng);
        let req_pub = requester.verifying_key().to_bytes();
        let req_id = NodeId::from_pubkey(&req_pub);
        let now = p2p_trust::now_ts();
        let finance_token = {
            let t = CapabilityToken::mint(
                &issuer,
                &req_pub,
                vec![
                    Caveat::Group("finance".into()),
                    Caveat::ExpiresAt(now + 100_000),
                ],
            );
            serde_json::to_string(&t).unwrap()
        };

        // A base valid private config that ROSTERS the requester.
        let base_private = |roster: Vec<String>, groups: Vec<&str>| {
            let mut cfg = GridConfig::default();
            cfg.security.mode = p2p_config::SecurityMode::Private;
            cfg.identity.pinning_mode = p2p_config::PinningMode::Allowlist;
            cfg.identity.allowlist = roster;
            cfg.membership.networks = vec!["acme-internal".into()];
            cfg.membership.groups = groups.into_iter().map(String::from).collect();
            cfg.membership.group_enforcement = p2p_config::GroupEnforcement::Token;
            cfg.membership
                .group_issuers
                .insert("finance".into(), issuer_hex.clone());
            cfg.validate().unwrap();
            cfg
        };
        let offer = |proof: Option<String>| Offer {
            job_id: JobId::new(),
            requester_id: req_id.clone(),
            query_hash: QueryHash::compute("SELECT 1", "v"),
            cost_hint_rows: Some(1),
            cost_hint_bytes: None,
            data_class: DataClass::Public,
            nonce: 0,
            network: Some("acme-internal".into()),
            groups: Vec::new(),
            regions: Vec::new(),
            group_proof: proof,
            input_fingerprint_hint: None,
        };
        let accept = |b: &p2p_proto::Bid| matches!(b.decision, BidDecision::Accept);
        let reject = |b: &p2p_proto::Bid| matches!(b.decision, BidDecision::Reject { .. });

        // 1. Properly-tokened company node ON the roster ⇒ accepted.
        let cfg = base_private(vec![req_id.0.clone()], vec!["finance"]);
        let node = Node::with_config(cfg, Arc::new(MockEngine::deterministic())).unwrap();
        let w = node.host_worker();
        assert!(accept(
            &w.bid_for_peer(&req_id, &offer(Some(finance_token.clone())))
        ));

        // 2. No group token ⇒ rejected (soft declared groups never count).
        assert!(reject(&w.bid_for_peer(&req_id, &offer(None))));

        // 3. UNGROUPED host (require_grouped_hosts) ⇒ rejects every offer.
        let cfg = base_private(vec![req_id.0.clone()], vec![]);
        let node = Node::with_config(cfg, Arc::new(MockEngine::deterministic())).unwrap();
        let w_ungrouped = node.host_worker();
        assert!(reject(
            &w_ungrouped.bid_for_peer(&req_id, &offer(Some(finance_token.clone())))
        ));

        // 4. UNROSTERED requester (a valid member id, but not the requester) ⇒
        //    default-deny rejects it even with a valid token.
        let other_id = format!("b3:{}", "0".repeat(64));
        let cfg = base_private(vec![other_id], vec!["finance"]);
        let node = Node::with_config(cfg, Arc::new(MockEngine::deterministic())).unwrap();
        let w_roster = node.host_worker();
        assert!(reject(
            &w_roster.bid_for_peer(&req_id, &offer(Some(finance_token)))
        ));
    }

    #[tokio::test]
    async fn host_refuses_remote_access_without_egress_confinement() {
        // Fail-safe guard (§9.4, G8): a host that enabled remote object-store
        // reads but has NO active OS egress filter (no `P2P_JOB_EXEC` ⇒ in-process,
        // unfiltered egress) must REFUSE every offer rather than allow open egress
        // (an SSRF footgun). It does not crash and keeps the DuckDB lockdown.
        let mk_offer = || Offer {
            job_id: JobId::new(),
            requester_id: NodeId("b3:req".into()),
            query_hash: QueryHash::compute("SELECT 1", "v"),
            cost_hint_rows: Some(1),
            cost_hint_bytes: None,
            data_class: DataClass::Public,
            nonce: 0,
            network: None,
            groups: Vec::new(),
            regions: Vec::new(),
            group_proof: None,
            input_fingerprint_hint: None,
        };

        let mut cfg = GridConfig::default(); // sandbox enabled + process_per_job
        cfg.storage.enable_remote_access = true;
        cfg.validate().unwrap();
        let node = Node::with_config(cfg, Arc::new(MockEngine::deterministic())).unwrap();
        assert!(
            !node.host_remote_egress_confined(),
            "no P2P_JOB_EXEC ⇒ egress is not confined"
        );
        let worker = node.host_worker();
        assert!(
            matches!(
                worker.bid_for(&mk_offer()).decision,
                BidDecision::Reject { .. }
            ),
            "remote-access host without egress confinement must refuse to serve"
        );

        // The requester's OWN local self-run path is UNAFFECTED (no protection
        // from yourself): a local query still runs unconfined and returns a row.
        let outcome = node
            .query("SELECT 1 AS x", p2p_config::QueryOverrides::default())
            .await
            .unwrap();
        assert!(
            outcome.executed_locally,
            "self-run local path must stay usable for the requester"
        );

        // A host WITHOUT remote access serves normally (the guard is conditional,
        // not a blanket refusal) — same secure defaults, no open-egress risk.
        let node2 = Node::with_config(GridConfig::default(), Arc::new(MockEngine::deterministic()))
            .unwrap();
        assert!(matches!(
            node2.host_worker().bid_for(&mk_offer()).decision,
            BidDecision::Accept
        ));
    }

    #[tokio::test]
    async fn host_sandbox_is_secure_by_default_but_self_run_is_not_sandboxed() {
        // Task #2: the secure sandbox posture is HOST-only. The global default is
        // neutral (off), so a pure requester is never force-sandboxed; the host
        // serving path applies the secure posture at spawn_host.
        let node = Node::with_config(GridConfig::default(), Arc::new(MockEngine::deterministic()))
            .unwrap();

        // Global default is neutral (the requester's self-run posture).
        assert!(!node.config().sandbox.enabled);
        assert!(!node.config().sandbox.process_per_job);

        // The HOST-serving path is secure-by-default (sandbox + process-per-job).
        let host = node.host_sandbox();
        assert!(host.enabled && host.process_per_job);

        // The requester's OWN local self-run path runs in-process and is never
        // sandboxed — a local query still returns a row under the neutral default.
        let outcome = node
            .query("SELECT 1 AS x", p2p_config::QueryOverrides::default())
            .await
            .unwrap();
        assert!(outcome.executed_locally, "self-run must stay in-process");
    }

    #[tokio::test]
    async fn host_sandbox_honors_explicit_operator_optout() {
        // An explicit `[sandbox]` customization is honored verbatim for the host
        // (not overridden by the secure host defaults) — e.g. backend = none opts
        // a host out of the OS sandbox.
        let mut cfg = GridConfig::default();
        cfg.sandbox.backend = p2p_config::SandboxBackend::None;
        cfg.validate().unwrap();
        let node = Node::with_config(cfg, Arc::new(MockEngine::deterministic())).unwrap();
        let host = node.host_sandbox();
        assert_eq!(host.backend, p2p_config::SandboxBackend::None);
        assert!(!host.enabled, "operator opt-out is respected on the host");
    }

    #[test]
    fn preflight_estimate_only_for_pure_in_memory_queries() {
        // Pure scalar / no data source ⇒ a (tiny) estimate so `auto` stays local.
        let e = preflight_estimate("SELECT 1 + 1").expect("pure scalar gets an estimate");
        assert_eq!(e.peak_working_set_bytes, 0);
        assert!(preflight_estimate("SELECT now()").is_some());

        // Anything with a data source ⇒ None (today's routing preserved).
        assert!(preflight_estimate("SELECT * FROM t").is_none());
        assert!(preflight_estimate("select count(*)\nfrom read_csv_auto('x.csv')").is_none());
        assert!(preflight_estimate("SELECT a FROM range(100)").is_none());
    }
}
