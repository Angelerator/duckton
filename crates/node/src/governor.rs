//! Process-wide capacity governor (architecture §10 — dual-role oversubscription).
//!
//! A node is symmetric: it can run its OWN queries on the free local path
//! ([`crate::planner::LocalExecutor`]) **and** serve OTHERS' jobs as a host
//! ([`crate::admission::AdmissionController`]) at the same time. Historically
//! those two pools accounted resources *independently*, so the sum of "own" +
//! "served" concurrent work could exceed the machine's donated `budget` and
//! oversubscribe physical RAM / CPU.
//!
//! The [`CapacityGovernor`] is the single, shared, process-wide resource pool
//! both roles reserve from BEFORE running anything. It enforces the hard machine
//! cap as an invariant:
//!
//! ```text
//! (memory reserved by own) + (memory reserved by served) <= budget.memory_bytes
//! (threads reserved by own) + (threads reserved by served) <= budget.threads
//! (own concurrent jobs) + (served concurrent jobs) <= max_concurrent_jobs
//! ```
//!
//! The per-role semaphores/atomics in `LocalExecutor` and `AdmissionController`
//! still apply (layered on top); the governor is the additional hard cap that
//! makes oversubscription across the two roles impossible.
//!
//! ## Own-vs-served fairness (no starvation)
//!
//! To stop served jobs from consuming 100% of memory and locking the node out of
//! running its own queries, the governor caps the memory held by **served** jobs
//! at `(1 - local_reserved_fraction) * budget.memory_bytes`. That permanently
//! reserves `local_reserved_fraction * budget.memory_bytes` of headroom that only
//! the node's own (local) work can claim. The symmetric direction — local work
//! not starving serving — is provided by the layered `LocalExecutor` budget
//! (`planner.ram_fraction * budget.memory_bytes`), which bounds how much of the
//! shared pool own queries can ever hold, leaving the remainder for serving.
//!
//! The served ceiling is only applied when local execution is actually active for
//! this node; a serve-only node sees the full budget (back-compat), and a
//! local-only node is bounded by its `LocalExecutor` budget as before.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use p2p_config::BudgetConfig;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Which role a reservation is made on behalf of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The node running its OWN query on the free local path.
    Local,
    /// The node SERVING another requester's job as a host/worker.
    Served,
}

/// Single shared, process-wide resource pool both roles reserve from.
pub struct CapacityGovernor {
    memory_total: u64,
    threads_total: u32,
    /// Hard ceiling on memory held by SERVED jobs. The complement
    /// (`memory_total - served_memory_ceiling`) is permanently reserved for the
    /// node's own (local) work so serving can never lock local out.
    served_memory_ceiling: u64,
    /// Memory reserved across BOTH roles (enforces the SUM invariant).
    used_memory: AtomicU64,
    /// Threads reserved across BOTH roles.
    used_threads: AtomicU32,
    /// Memory reserved by SERVED jobs only (enforced against the ceiling).
    served_memory: AtomicU64,
    /// Global cap on the number of concurrent jobs across both roles
    /// (`limits.worker_pool_size`).
    job_slots: Arc<Semaphore>,
    max_jobs: usize,
}

impl CapacityGovernor {
    /// Build the governor from the node's donated [`BudgetConfig`], the global
    /// max-concurrent-jobs cap (`limits.worker_pool_size`), the configured
    /// `local_reserved_fraction`, and whether local execution is active.
    ///
    /// When `local_active` is `false` the reserved-for-local headroom collapses to
    /// zero, so a serve-only node gets the full budget (back-compat).
    pub fn new(
        budget: &BudgetConfig,
        max_concurrent_jobs: usize,
        local_reserved_fraction: f64,
        local_active: bool,
    ) -> Arc<Self> {
        let fraction = if local_active {
            local_reserved_fraction.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let served_memory_ceiling =
            ((budget.memory_bytes as f64) * (1.0 - fraction)).floor() as u64;
        let max_jobs = max_concurrent_jobs.max(1);
        Arc::new(Self {
            memory_total: budget.memory_bytes,
            threads_total: budget.threads,
            served_memory_ceiling,
            used_memory: AtomicU64::new(0),
            used_threads: AtomicU32::new(0),
            served_memory: AtomicU64::new(0),
            job_slots: Arc::new(Semaphore::new(max_jobs)),
            max_jobs,
        })
    }

    /// Total memory the governor manages (the donated budget).
    pub fn memory_total(&self) -> u64 {
        self.memory_total
    }

    /// Hard ceiling on memory that served jobs may collectively hold.
    pub fn served_memory_ceiling(&self) -> u64 {
        self.served_memory_ceiling
    }

    /// Global max concurrent jobs across both roles.
    pub fn max_jobs(&self) -> usize {
        self.max_jobs
    }

    /// Memory currently free in the shared pool (across both roles).
    pub fn free_memory(&self) -> u64 {
        self.memory_total
            .saturating_sub(self.used_memory.load(Ordering::Relaxed))
    }

    /// Threads currently free in the shared pool (across both roles).
    pub fn free_threads(&self) -> u32 {
        self.threads_total
            .saturating_sub(self.used_threads.load(Ordering::Relaxed))
    }

    /// Job slots currently free in the shared pool.
    pub fn available_slots(&self) -> usize {
        self.job_slots.available_permits()
    }

    /// Try to reserve one job slot plus `memory`/`threads` for `role`. Returns a
    /// [`GovernorLease`] that releases everything on drop, or `None` if the
    /// reservation would breach the machine cap (or, for [`Role::Served`], the
    /// served memory ceiling).
    pub fn try_reserve(
        self: &Arc<Self>,
        role: Role,
        memory: u64,
        threads: u32,
    ) -> Option<GovernorLease> {
        // 1. Global concurrency slot (bounds total in-flight jobs across roles).
        let permit = Arc::clone(&self.job_slots).try_acquire_owned().ok()?;

        // 2. Total memory (the SUM invariant across both roles).
        let mut cur = self.used_memory.load(Ordering::Relaxed);
        loop {
            let next = cur.checked_add(memory)?; // permit dropped here on None
            if next > self.memory_total {
                return None;
            }
            match self.used_memory.compare_exchange_weak(
                cur,
                next,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => cur = observed,
            }
        }

        // 3. Served-only ceiling (reserves headroom for the node's own work).
        if role == Role::Served {
            let mut cur_s = self.served_memory.load(Ordering::Relaxed);
            loop {
                let next = match cur_s.checked_add(memory) {
                    Some(n) if n <= self.served_memory_ceiling => n,
                    _ => {
                        self.used_memory.fetch_sub(memory, Ordering::AcqRel);
                        return None;
                    }
                };
                match self.served_memory.compare_exchange_weak(
                    cur_s,
                    next,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(observed) => cur_s = observed,
                }
            }
        }

        // 4. Threads (the SUM invariant across both roles).
        let mut cur_t = self.used_threads.load(Ordering::Relaxed);
        loop {
            let next = match cur_t.checked_add(threads) {
                Some(n) if n <= self.threads_total => n,
                _ => {
                    self.rollback_memory(role, memory);
                    return None;
                }
            };
            match self.used_threads.compare_exchange_weak(
                cur_t,
                next,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => cur_t = observed,
            }
        }

        Some(GovernorLease {
            governor: Arc::clone(self),
            role,
            memory,
            threads,
            _permit: permit,
        })
    }

    fn rollback_memory(&self, role: Role, memory: u64) {
        self.used_memory.fetch_sub(memory, Ordering::AcqRel);
        if role == Role::Served {
            self.served_memory.fetch_sub(memory, Ordering::AcqRel);
        }
    }
}

/// A held governor reservation; returns its memory/threads/slot to the shared
/// pool on drop.
pub struct GovernorLease {
    governor: Arc<CapacityGovernor>,
    role: Role,
    memory: u64,
    threads: u32,
    _permit: OwnedSemaphorePermit,
}

impl GovernorLease {
    pub fn role(&self) -> Role {
        self.role
    }
    pub fn memory_bytes(&self) -> u64 {
        self.memory
    }
    pub fn threads(&self) -> u32 {
        self.threads
    }
}

impl Drop for GovernorLease {
    fn drop(&mut self) {
        self.governor.rollback_memory(self.role, self.memory);
        self.governor
            .used_threads
            .fetch_sub(self.threads, Ordering::AcqRel);
        // job-slot permit released automatically
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p2p_config::DataClassCfg;

    fn budget(mem: u64, threads: u32) -> BudgetConfig {
        BudgetConfig {
            memory_bytes: mem,
            threads,
            max_jobs: 8,
            per_job_memory_bytes: 100,
            per_job_threads: 1,
            local_reserved_fraction: 0.0,
            data_classes: vec![DataClassCfg::Public],
        }
    }

    #[test]
    fn sum_of_roles_cannot_exceed_memory_cap() {
        // No reservation for local (fraction 0) ⇒ pure SUM cap across roles.
        let g = CapacityGovernor::new(&budget(1000, 8), 8, 0.0, true);
        let _served = g.try_reserve(Role::Served, 600, 1).unwrap();
        let _local = g.try_reserve(Role::Local, 400, 1).unwrap();
        // 600 + 400 == 1000 exactly; one more byte must be refused on EITHER role.
        assert!(g.try_reserve(Role::Local, 1, 0).is_none());
        assert!(g.try_reserve(Role::Served, 1, 0).is_none());
        assert_eq!(g.free_memory(), 0);
    }

    #[test]
    fn sum_of_roles_cannot_exceed_thread_cap() {
        let g = CapacityGovernor::new(&budget(10_000, 3), 8, 0.0, true);
        let _served = g.try_reserve(Role::Served, 0, 2).unwrap();
        let _local = g.try_reserve(Role::Local, 0, 1).unwrap();
        assert!(g.try_reserve(Role::Local, 0, 1).is_none());
        assert_eq!(g.free_threads(), 0);
    }

    #[test]
    fn global_slot_cap_bounds_total_concurrency() {
        // max_concurrent_jobs = 2 regardless of remaining mem/threads.
        let g = CapacityGovernor::new(&budget(1_000_000, 64), 2, 0.0, true);
        let _a = g.try_reserve(Role::Local, 1, 1).unwrap();
        let _b = g.try_reserve(Role::Served, 1, 1).unwrap();
        assert!(g.try_reserve(Role::Local, 1, 1).is_none());
        assert_eq!(g.available_slots(), 0);
    }

    #[test]
    fn served_ceiling_reserves_headroom_for_local() {
        // 20% reserved for local ⇒ served may hold at most 800 of 1000.
        let g = CapacityGovernor::new(&budget(1000, 8), 8, 0.2, true);
        assert_eq!(g.served_memory_ceiling(), 800);
        let _served = g.try_reserve(Role::Served, 800, 1).unwrap();
        // Served is at its ceiling: a further served reservation is refused…
        assert!(g.try_reserve(Role::Served, 1, 0).is_none());
        // …but the reserved 200 is still available to LOCAL (no starvation).
        let _local = g.try_reserve(Role::Local, 200, 1).unwrap();
        assert_eq!(g.free_memory(), 0);
    }

    #[test]
    fn serve_only_node_gets_full_budget() {
        // local_active = false ⇒ the reservation collapses; served gets all 1000.
        let g = CapacityGovernor::new(&budget(1000, 8), 8, 0.2, false);
        assert_eq!(g.served_memory_ceiling(), 1000);
        let _served = g.try_reserve(Role::Served, 1000, 1).unwrap();
        assert_eq!(g.free_memory(), 0);
    }

    #[test]
    fn lease_drop_restores_pool() {
        let g = CapacityGovernor::new(&budget(1000, 4), 4, 0.2, true);
        {
            let _served = g.try_reserve(Role::Served, 800, 2).unwrap();
            let _local = g.try_reserve(Role::Local, 200, 2).unwrap();
            assert_eq!(g.free_memory(), 0);
            assert_eq!(g.free_threads(), 0);
            assert_eq!(g.available_slots(), 2);
        }
        // Everything (memory + threads + slots + served counter) is restored.
        assert_eq!(g.free_memory(), 1000);
        assert_eq!(g.free_threads(), 4);
        assert_eq!(g.available_slots(), 4);
        // The served counter was restored, so served can fill its ceiling again.
        let _served = g.try_reserve(Role::Served, 800, 1).unwrap();
    }
}
