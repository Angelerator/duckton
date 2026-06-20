//! Local-vs-remote query planner (architecture §4 data plane, §11 scheduler).
//!
//! Decides whether a query runs on the **free local path** (the node's own
//! locked-down in-process DuckDB — no bidding/escrow/quorum/payment, because a
//! node trusts its own machine) or is **dispatched to the grid** (hedged,
//! quorum-verified). The decision combines:
//!
//! * the per-call / configured **preference** (`local` | `remote` | `auto`),
//! * a pre-flight [`crate::estimator::WorkingSetEstimate`] (estimated peak RAM),
//! * the node's **current local headroom** (`budget.memory_bytes * ram_fraction`
//!   minus memory already in use by concurrent local jobs),
//! * a **spill tolerance** (how far the estimate may exceed RAM headroom while
//!   relying on DuckDB's out-of-core spill) and a **latency budget**,
//! * **local saturation** (no free local job slot ⇒ go remote).
//!
//! The planner is a pluggable trait ([`LocalOrRemotePlanner`]) consistent with
//! the project's other collaborators (`QueryEngine`, `Discovery`, `TrustStore`),
//! with a config-driven [`DefaultPlanner`]. Resource accounting + the free local
//! execution slot live in [`LocalExecutor`].

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use p2p_config::{PlannerConfig, PreferMode};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::engine::QueryEngine;
use crate::estimator::WorkingSetEstimate;
use crate::governor::{CapacityGovernor, GovernorLease, Role};

/// Where the planner decided a query should run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Run on the free, in-process, locked-down local engine.
    Local,
    /// Dispatch to the grid (hedged/quorum).
    Remote,
}

/// Why the planner chose a route (for logging/observability/tests).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanReason {
    /// Forced by preference (`prefer => local`).
    ForcedLocal,
    /// Forced by preference (`prefer => remote`), or planner disabled.
    ForcedRemote,
    /// Remote-only mode: local execution is disabled
    /// (`planner.local_execution_enabled = false`), so the query is dispatched
    /// to the grid regardless of preference or size — the node never runs a
    /// query on its own machine.
    LocalDisabled,
    /// Auto: estimate fits within headroom (+ spill tolerance) and budgets.
    FitsLocal,
    /// Auto: estimated peak working set exceeds headroom + spill tolerance.
    TooLarge,
    /// Auto: estimated scanned bytes exceed the absolute local size threshold.
    OverSizeThreshold,
    /// Auto: estimated local runtime exceeds the latency budget.
    OverLatencyBudget,
    /// Auto: all local job slots are in use (locally saturated).
    LocallySaturated,
    /// Auto: no estimate available, so route conservatively to the grid.
    NoEstimate,
}

/// A planner decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanDecision {
    pub route: Route,
    pub reason: PlanReason,
}

impl PlanDecision {
    pub fn local(reason: PlanReason) -> Self {
        Self {
            route: Route::Local,
            reason,
        }
    }
    pub fn remote(reason: PlanReason) -> Self {
        Self {
            route: Route::Remote,
            reason,
        }
    }
    pub fn is_local(&self) -> bool {
        self.route == Route::Local
    }
}

/// Inputs to a routing decision.
#[derive(Debug, Clone)]
pub struct PlanRequest {
    /// Effective preference (resolved from `planner.prefer` + per-call override).
    pub prefer: PreferMode,
    /// Pre-flight working-set estimate, if one could be computed. `None` means
    /// the data size is unknown (e.g. no metadata access for this source).
    pub estimate: Option<WorkingSetEstimate>,
    /// Current local RAM headroom in bytes (`budget*alpha − in_use`).
    pub headroom_bytes: u64,
    /// Whether a free local job slot is currently available.
    pub local_slot_available: bool,
}

/// Pluggable local-vs-remote planner.
pub trait LocalOrRemotePlanner: Send + Sync {
    /// Decide where a query should run.
    fn decide(&self, req: &PlanRequest) -> PlanDecision;

    /// After a query that started **locally** fails by exhausting its resource
    /// budget mid-flight (adaptive fail-over), decide whether to re-dispatch it
    /// to the grid. `prefer` is the effective preference for the call.
    fn failover_to_remote(&self, prefer: PreferMode) -> bool {
        // Default policy: fail over unless the caller explicitly pinned `local`
        // (in which case respect their choice and surface the local error).
        !matches!(prefer, PreferMode::Local)
    }
}

/// The default, config-driven planner.
#[derive(Debug, Clone)]
pub struct DefaultPlanner {
    cfg: PlannerConfig,
}

impl DefaultPlanner {
    pub fn new(cfg: PlannerConfig) -> Self {
        Self { cfg }
    }

    pub fn config(&self) -> &PlannerConfig {
        &self.cfg
    }
}

impl LocalOrRemotePlanner for DefaultPlanner {
    fn decide(&self, req: &PlanRequest) -> PlanDecision {
        // Master switch: planner off ⇒ behave exactly like before (grid only).
        if !self.cfg.enabled {
            return PlanDecision::remote(PlanReason::ForcedRemote);
        }

        // Remote-only mode (hard gate): when local execution is disabled the node
        // NEVER runs a query on its own machine — not even a tiny one that would
        // fit, and not even when the caller pinned `prefer => 'local'`. Every
        // query is dispatched to the grid and the adaptive "start local" path is
        // skipped entirely (the coordinator never reserves a local slot).
        if !self.cfg.local_execution_enabled {
            return PlanDecision::remote(PlanReason::LocalDisabled);
        }

        match req.prefer {
            // "You trust your own machine": forced local, no estimate needed.
            PreferMode::Local => return PlanDecision::local(PlanReason::ForcedLocal),
            PreferMode::Remote => return PlanDecision::remote(PlanReason::ForcedRemote),
            PreferMode::Auto => {}
        }

        // Auto mode below.
        // Locally saturated ⇒ grid.
        if !req.local_slot_available {
            return PlanDecision::remote(PlanReason::LocallySaturated);
        }

        // Without an estimate we can't bound RAM ⇒ route conservatively remote.
        let est = match &req.estimate {
            Some(e) => e,
            None => return PlanDecision::remote(PlanReason::NoEstimate),
        };

        // Absolute scanned-bytes cap (guards against pathological scans).
        if est.scanned_uncompressed_bytes > self.cfg.size_threshold_bytes {
            return PlanDecision::remote(PlanReason::OverSizeThreshold);
        }

        // Latency budget (0 disables the gate).
        if self.cfg.max_local_latency_ms > 0
            && est.estimated_runtime_ms > self.cfg.max_local_latency_ms
        {
            return PlanDecision::remote(PlanReason::OverLatencyBudget);
        }

        // Peak working set must fit RAM headroom, allowing spill tolerance.
        let allowance = self
            .cfg
            .spill_tolerance_bytes
            .saturating_add(req.headroom_bytes);
        if est.peak_working_set_bytes > allowance {
            return PlanDecision::remote(PlanReason::TooLarge);
        }

        PlanDecision::local(PlanReason::FitsLocal)
    }
}

/// Free local execution engine + headroom / concurrency accounting.
///
/// Mirrors the worker-side [`crate::admission::AdmissionController`] pattern: a
/// semaphore bounds concurrent local jobs and an atomic tracks RAM reserved by
/// in-flight local jobs, so the planner can compute *current* headroom rather
/// than a static budget.
pub struct LocalExecutor {
    engine: Arc<dyn QueryEngine>,
    /// Total RAM the node will devote to local execution = `budget * alpha`.
    local_budget_bytes: u64,
    used_bytes: AtomicU64,
    slots: Arc<Semaphore>,
    max_jobs: usize,
    /// Threads each local job reserves from the process-wide governor.
    job_threads: u32,
    /// Floor applied to each reservation's byte count so a no-/tiny-estimate
    /// local job still declares a representative footprint to the governor and
    /// the local-budget CAS (otherwise own queries would account ~0 against the
    /// process-wide cap). `0` for the standalone executor (no floor → today's
    /// accounting, used by unit tests).
    min_reservation_bytes: u64,
    /// Process-wide capacity governor shared with the worker-side
    /// [`crate::admission::AdmissionController`]. When set, each local
    /// reservation also claims from the governor so own + served work cannot
    /// jointly oversubscribe the machine. `None` ⇒ standalone (today's behavior;
    /// used by unit tests and single-component setups).
    governor: Option<Arc<CapacityGovernor>>,
}

impl LocalExecutor {
    /// Build a standalone executor (no process-wide governor) from the engine,
    /// the node's total memory budget (bytes) and the planner config
    /// (`ram_fraction` = alpha, `max_concurrent_local_jobs`).
    pub fn new(
        engine: Arc<dyn QueryEngine>,
        budget_memory_bytes: u64,
        cfg: &PlannerConfig,
    ) -> Arc<Self> {
        Self::build(engine, budget_memory_bytes, cfg, 1, 0, None)
    }

    /// Build an executor wired to the shared process-wide [`CapacityGovernor`]
    /// (the dual-role path): each local reservation also reserves `job_threads`
    /// and at least `per_job_memory_bytes` from the governor, so own queries are
    /// accounted against the same hard cap as served jobs.
    pub fn governed(
        engine: Arc<dyn QueryEngine>,
        budget_memory_bytes: u64,
        cfg: &PlannerConfig,
        job_threads: u32,
        per_job_memory_bytes: u64,
        governor: Arc<CapacityGovernor>,
    ) -> Arc<Self> {
        Self::build(
            engine,
            budget_memory_bytes,
            cfg,
            job_threads,
            per_job_memory_bytes,
            Some(governor),
        )
    }

    fn build(
        engine: Arc<dyn QueryEngine>,
        budget_memory_bytes: u64,
        cfg: &PlannerConfig,
        job_threads: u32,
        min_reservation_bytes: u64,
        governor: Option<Arc<CapacityGovernor>>,
    ) -> Arc<Self> {
        let local_budget_bytes = ((budget_memory_bytes as f64) * cfg.ram_fraction) as u64;
        let max_jobs = cfg.max_concurrent_local_jobs.max(1);
        Arc::new(Self {
            engine,
            local_budget_bytes,
            used_bytes: AtomicU64::new(0),
            slots: Arc::new(Semaphore::new(max_jobs)),
            max_jobs,
            job_threads: job_threads.max(1),
            min_reservation_bytes,
            governor,
        })
    }

    pub fn engine(&self) -> &Arc<dyn QueryEngine> {
        &self.engine
    }

    pub fn local_budget_bytes(&self) -> u64 {
        self.local_budget_bytes
    }

    pub fn max_jobs(&self) -> usize {
        self.max_jobs
    }

    /// Current RAM headroom = `local_budget − in_use`.
    pub fn headroom_bytes(&self) -> u64 {
        self.local_budget_bytes
            .saturating_sub(self.used_bytes.load(Ordering::Relaxed))
    }

    /// Is a free local job slot available right now?
    pub fn slot_available(&self) -> bool {
        self.slots.available_permits() > 0
    }

    /// Reserve a local execution slot + `reserve_bytes` of headroom. Returns a
    /// [`LocalReservation`] that releases both on drop, or `None` if no slot is
    /// free (locally saturated), if the reservation would exceed the local
    /// budget, or if the shared [`CapacityGovernor`] is at capacity.
    ///
    /// The memory reservation is committed with a hard compare-and-swap against
    /// `local_budget_bytes` (mirroring the worker-side admission CAS), so racing
    /// concurrent local jobs can never over-account past the local budget — the
    /// planner pre-check alone could let two jobs that each fit individually both
    /// commit and oversubscribe.
    pub fn reserve(self: &Arc<Self>, reserve_bytes: u64) -> Option<LocalReservation> {
        let reserve_bytes = reserve_bytes.max(self.min_reservation_bytes);
        let permit = Arc::clone(&self.slots).try_acquire_owned().ok()?;

        // Hard CAS against the local budget (so concurrent local jobs can't
        // over-account). On failure the slot permit is released by `permit`'s
        // drop and the job is routed to the grid.
        let mut cur = self.used_bytes.load(Ordering::Relaxed);
        loop {
            let next = cur.checked_add(reserve_bytes)?;
            if next > self.local_budget_bytes {
                return None;
            }
            match self.used_bytes.compare_exchange_weak(
                cur,
                next,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => cur = observed,
            }
        }

        // Process-wide governor: own work shares the hard machine cap with
        // served jobs. If the governor is at capacity, undo the local accounting
        // and route to the grid.
        let governor_lease = match &self.governor {
            Some(g) => match g.try_reserve(Role::Local, reserve_bytes, self.job_threads) {
                Some(lease) => Some(lease),
                None => {
                    self.used_bytes.fetch_sub(reserve_bytes, Ordering::AcqRel);
                    return None;
                }
            },
            None => None,
        };

        Some(LocalReservation {
            owner: Arc::clone(self),
            reserved_bytes: reserve_bytes,
            _permit: permit,
            _governor_lease: governor_lease,
        })
    }
}

/// A held local execution reservation; releases its RAM + slot (and the shared
/// governor reservation, if any) on drop.
pub struct LocalReservation {
    owner: Arc<LocalExecutor>,
    reserved_bytes: u64,
    _permit: OwnedSemaphorePermit,
    /// Released first (before the local accounting below) on drop.
    _governor_lease: Option<GovernorLease>,
}

impl Drop for LocalReservation {
    fn drop(&mut self) {
        self.owner
            .used_bytes
            .fetch_sub(self.reserved_bytes, Ordering::AcqRel);
    }
}

/// Heuristic: does an engine error look like a resource-exhaustion (OOM /
/// memory-limit / out-of-memory) failure that should trigger adaptive
/// fail-over to the grid? (DuckDB surfaces these as "Out of Memory Error".)
pub fn is_resource_exhaustion(err: &crate::engine::EngineError) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("out of memory")
        || msg.contains("could not allocate")
        || msg.contains("memory limit")
        || msg.contains("failed to allocate")
        || msg.contains("exceeds the memory limit")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::estimator::WorkingSetEstimate;

    fn ws(peak: u64, scanned: u64, runtime_ms: u64) -> WorkingSetEstimate {
        WorkingSetEstimate {
            scanned_uncompressed_bytes: scanned,
            estimated_rows: 0,
            scan_buffer_bytes: 0,
            group_by_bytes: 0,
            join_build_bytes: 0,
            sort_bytes: 0,
            peak_working_set_bytes: peak,
            estimated_runtime_ms: runtime_ms,
        }
    }

    fn cfg() -> PlannerConfig {
        PlannerConfig {
            enabled: true,
            local_execution_enabled: true,
            prefer: PreferMode::Auto,
            ram_fraction: 0.6,
            max_concurrent_local_jobs: 4,
            size_threshold_bytes: 256 * 1024 * 1024,
            spill_tolerance_bytes: 0,
            max_local_latency_ms: 10_000,
        }
    }

    #[test]
    fn fits_goes_local() {
        let p = DefaultPlanner::new(cfg());
        let d = p.decide(&PlanRequest {
            prefer: PreferMode::Auto,
            estimate: Some(ws(50_000_000, 50_000_000, 100)),
            headroom_bytes: 100_000_000,
            local_slot_available: true,
        });
        assert_eq!(d, PlanDecision::local(PlanReason::FitsLocal));
    }

    #[test]
    fn too_big_for_headroom_goes_remote() {
        let p = DefaultPlanner::new(cfg());
        let d = p.decide(&PlanRequest {
            prefer: PreferMode::Auto,
            estimate: Some(ws(200_000_000, 100_000_000, 100)),
            headroom_bytes: 100_000_000,
            local_slot_available: true,
        });
        assert_eq!(d.route, Route::Remote);
        assert_eq!(d.reason, PlanReason::TooLarge);
    }

    #[test]
    fn spill_tolerance_allows_slight_overflow_local() {
        let mut c = cfg();
        c.spill_tolerance_bytes = 50_000_000;
        let p = DefaultPlanner::new(c);
        // peak 130M, headroom 100M, tolerance 50M → 130 <= 150 → local.
        let d = p.decide(&PlanRequest {
            prefer: PreferMode::Auto,
            estimate: Some(ws(130_000_000, 100_000_000, 100)),
            headroom_bytes: 100_000_000,
            local_slot_available: true,
        });
        assert!(d.is_local());
    }

    #[test]
    fn over_size_threshold_goes_remote() {
        let mut c = cfg();
        c.size_threshold_bytes = 10_000_000;
        let p = DefaultPlanner::new(c);
        let d = p.decide(&PlanRequest {
            prefer: PreferMode::Auto,
            estimate: Some(ws(1_000, 20_000_000, 1)),
            headroom_bytes: u64::MAX,
            local_slot_available: true,
        });
        assert_eq!(d.reason, PlanReason::OverSizeThreshold);
    }

    #[test]
    fn latency_budget_routes_remote() {
        let p = DefaultPlanner::new(cfg());
        let d = p.decide(&PlanRequest {
            prefer: PreferMode::Auto,
            estimate: Some(ws(1_000, 1_000, 999_999)),
            headroom_bytes: u64::MAX,
            local_slot_available: true,
        });
        assert_eq!(d.reason, PlanReason::OverLatencyBudget);
    }

    #[test]
    fn locally_saturated_goes_remote() {
        let p = DefaultPlanner::new(cfg());
        let d = p.decide(&PlanRequest {
            prefer: PreferMode::Auto,
            estimate: Some(ws(1, 1, 1)),
            headroom_bytes: u64::MAX,
            local_slot_available: false,
        });
        assert_eq!(d.reason, PlanReason::LocallySaturated);
    }

    #[test]
    fn no_estimate_in_auto_goes_remote() {
        let p = DefaultPlanner::new(cfg());
        let d = p.decide(&PlanRequest {
            prefer: PreferMode::Auto,
            estimate: None,
            headroom_bytes: u64::MAX,
            local_slot_available: true,
        });
        assert_eq!(d.reason, PlanReason::NoEstimate);
    }

    #[test]
    fn prefer_local_forces_local_even_without_estimate() {
        let p = DefaultPlanner::new(cfg());
        let d = p.decide(&PlanRequest {
            prefer: PreferMode::Local,
            estimate: None,
            headroom_bytes: 0,
            local_slot_available: true,
        });
        assert_eq!(d, PlanDecision::local(PlanReason::ForcedLocal));
    }

    #[test]
    fn remote_only_mode_forces_remote_even_for_fitting_tiny_query() {
        // local_execution_enabled = false ⇒ a tiny query that WOULD fit locally
        // is still routed to the grid.
        let mut c = cfg();
        c.local_execution_enabled = false;
        let p = DefaultPlanner::new(c);
        let d = p.decide(&PlanRequest {
            prefer: PreferMode::Auto,
            estimate: Some(ws(1, 1, 1)),
            headroom_bytes: u64::MAX,
            local_slot_available: true,
        });
        assert_eq!(d, PlanDecision::remote(PlanReason::LocalDisabled));
    }

    #[test]
    fn remote_only_mode_overrides_prefer_local() {
        // The hard gate beats an explicit `prefer => local` — the node never runs
        // a query on its own machine in remote-only mode.
        let mut c = cfg();
        c.local_execution_enabled = false;
        let p = DefaultPlanner::new(c);
        let d = p.decide(&PlanRequest {
            prefer: PreferMode::Local,
            estimate: None,
            headroom_bytes: u64::MAX,
            local_slot_available: true,
        });
        assert_eq!(d, PlanDecision::remote(PlanReason::LocalDisabled));
    }

    #[test]
    fn disabled_planner_always_remote() {
        let mut c = cfg();
        c.enabled = false;
        let p = DefaultPlanner::new(c);
        let d = p.decide(&PlanRequest {
            prefer: PreferMode::Local,
            estimate: Some(ws(1, 1, 1)),
            headroom_bytes: u64::MAX,
            local_slot_available: true,
        });
        assert_eq!(d, PlanDecision::remote(PlanReason::ForcedRemote));
    }

    #[test]
    fn failover_policy_respects_forced_local() {
        let p = DefaultPlanner::new(cfg());
        assert!(p.failover_to_remote(PreferMode::Auto));
        assert!(p.failover_to_remote(PreferMode::Remote));
        assert!(!p.failover_to_remote(PreferMode::Local));
    }

    #[test]
    fn local_executor_headroom_and_saturation() {
        let engine = Arc::new(crate::engine::MockEngine::deterministic()) as Arc<dyn QueryEngine>;
        let mut c = cfg();
        c.max_concurrent_local_jobs = 2;
        c.ram_fraction = 0.5;
        let ex = LocalExecutor::new(engine, 1_000, &c);
        assert_eq!(ex.local_budget_bytes(), 500);
        assert_eq!(ex.headroom_bytes(), 500);

        let r1 = ex.reserve(200).unwrap();
        assert_eq!(ex.headroom_bytes(), 300);
        let _r2 = ex.reserve(100).unwrap();
        assert_eq!(ex.headroom_bytes(), 200);
        // Only 2 slots → third reservation fails (saturated).
        assert!(!ex.slot_available());
        assert!(ex.reserve(1).is_none());
        // Dropping a reservation restores headroom + slot.
        drop(r1);
        assert_eq!(ex.headroom_bytes(), 400);
        assert!(ex.slot_available());
    }

    #[test]
    fn reserve_cas_rejects_over_budget_local_reservation() {
        // Gap #3: a reservation that would push `used_bytes` past the local
        // budget is rejected by the hard CAS even when a slot is free, so racing
        // concurrent local jobs can't over-account. local_budget = 0.5 * 1000.
        let engine = Arc::new(crate::engine::MockEngine::deterministic()) as Arc<dyn QueryEngine>;
        let mut c = cfg();
        c.ram_fraction = 0.5;
        c.max_concurrent_local_jobs = 8; // slots are NOT the binding constraint here
        let ex = LocalExecutor::new(engine, 1_000, &c);
        assert_eq!(ex.local_budget_bytes(), 500);

        let _r = ex.reserve(400).expect("400 <= 500 fits");
        // A free slot remains, but 400 + 200 > 500 ⇒ the CAS rejects.
        assert!(ex.slot_available());
        assert!(ex.reserve(200).is_none(), "over-budget reservation must be rejected");
        // A reservation that fits the remaining 100 still succeeds.
        let _r2 = ex.reserve(100).expect("400 + 100 == 500 fits exactly");
        assert_eq!(ex.headroom_bytes(), 0);
    }

    #[test]
    fn governed_reserve_claims_from_shared_governor_and_releases_on_drop() {
        use crate::governor::{CapacityGovernor, Role};
        use p2p_config::{BudgetConfig, DataClassCfg};

        let budget = BudgetConfig {
            memory_bytes: 1000,
            threads: 4,
            max_jobs: 4,
            per_job_memory_bytes: 100,
            per_job_threads: 1,
            local_reserved_fraction: 0.0,
            data_classes: vec![DataClassCfg::Public],
        };
        let governor = CapacityGovernor::new(&budget, 4, 0.0, true);
        let engine = Arc::new(crate::engine::MockEngine::deterministic()) as Arc<dyn QueryEngine>;
        let mut c = cfg();
        c.ram_fraction = 1.0; // local budget == full 1000 (governor is the cap)
        c.max_concurrent_local_jobs = 4;
        let ex = LocalExecutor::governed(engine, 1000, &c, 1, 0, Arc::clone(&governor));

        // Pre-occupy 800 of the governor on the SERVED role.
        let served = governor.try_reserve(Role::Served, 800, 1).unwrap();
        // Local fits in the remaining 200 of the shared pool…
        let r = ex.reserve(200).expect("200 fits the remaining governor headroom");
        assert_eq!(governor.free_memory(), 0);
        // …but a further local reservation is refused by the governor even though
        // the LOCAL budget (1000) would allow it — the shared cap binds.
        assert!(ex.reserve(1).is_none());
        // Dropping the local reservation returns its bytes to the shared pool.
        drop(r);
        assert_eq!(governor.free_memory(), 200);
        drop(served);
        assert_eq!(governor.free_memory(), 1000);
    }

    #[test]
    fn resource_exhaustion_detection() {
        use crate::engine::EngineError;
        assert!(is_resource_exhaustion(&EngineError::Exec(
            "Out of Memory Error: failed to allocate".into()
        )));
        assert!(is_resource_exhaustion(&EngineError::Exec(
            "could not allocate 4GB".into()
        )));
        assert!(!is_resource_exhaustion(&EngineError::Exec(
            "syntax error near SELECT".into()
        )));
    }
}
