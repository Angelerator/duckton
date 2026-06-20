//! Worker-side admission control & resource budgeting (architecture §10).
//!
//! Enforces the donated budget (memory / threads / max concurrent jobs) and
//! the allowed data classes. Concurrency is bounded by a semaphore (scalability:
//! never unbounded). A granted [`Lease`] restores the reserved resources on drop.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use p2p_config::{BudgetConfig, DataClassCfg};
use p2p_proto::DataClass;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::governor::{CapacityGovernor, GovernorLease, Role};

/// Tracks current usage against the configured budget.
pub struct AdmissionController {
    memory_bytes: u64,
    threads: u32,
    data_classes: Vec<DataClassCfg>,
    used_memory: AtomicU64,
    used_threads: AtomicU32,
    /// Caps concurrent admitted jobs (== max_jobs).
    job_slots: Arc<Semaphore>,
    max_jobs: u32,
    /// Process-wide capacity governor shared with the requester-side
    /// [`crate::planner::LocalExecutor`]. When set, an admitted job also reserves
    /// from the governor so served + own work cannot jointly oversubscribe the
    /// machine. `None` ⇒ standalone (today's behavior; used by tests and
    /// serve-only tooling that builds a controller directly).
    governor: Option<Arc<CapacityGovernor>>,
}

impl AdmissionController {
    /// Build a standalone controller (no process-wide governor).
    pub fn new(budget: &BudgetConfig) -> Arc<Self> {
        Self::build(budget, None)
    }

    /// Build a controller wired to the shared process-wide [`CapacityGovernor`]
    /// (the dual-role path): each admission also reserves from the governor.
    pub fn governed(budget: &BudgetConfig, governor: Arc<CapacityGovernor>) -> Arc<Self> {
        Self::build(budget, Some(governor))
    }

    fn build(budget: &BudgetConfig, governor: Option<Arc<CapacityGovernor>>) -> Arc<Self> {
        Arc::new(Self {
            memory_bytes: budget.memory_bytes,
            threads: budget.threads,
            data_classes: budget.data_classes.clone(),
            used_memory: AtomicU64::new(0),
            used_threads: AtomicU32::new(0),
            job_slots: Arc::new(Semaphore::new(budget.max_jobs as usize)),
            max_jobs: budget.max_jobs,
            governor,
        })
    }

    /// Whether this host serves the given data class.
    pub fn serves_data_class(&self, dc: DataClass) -> bool {
        let want = match dc {
            DataClass::Public => DataClassCfg::Public,
            DataClass::Internal => DataClassCfg::Internal,
            DataClass::Sensitive => DataClassCfg::Sensitive,
        };
        self.data_classes.contains(&want)
    }

    /// Snapshot of currently free resources (advertised in a Bid).
    pub fn free(&self) -> FreeResources {
        let used_mem = self.used_memory.load(Ordering::Relaxed);
        let used_threads = self.used_threads.load(Ordering::Relaxed);
        FreeResources {
            memory_bytes: self.memory_bytes.saturating_sub(used_mem),
            threads: self.threads.saturating_sub(used_threads),
            free_jobs: self.job_slots.available_permits() as u32,
        }
    }

    pub fn max_jobs(&self) -> u32 {
        self.max_jobs
    }

    /// Attempt to reserve resources for one job. Returns a [`Lease`] on success,
    /// or `None` if at capacity. Reservation is atomic w.r.t. the budget.
    pub fn try_admit(self: &Arc<Self>, memory_bytes: u64, threads: u32) -> Option<Lease> {
        // First take a job slot (bounds concurrency).
        let permit = Arc::clone(&self.job_slots).try_acquire_owned().ok()?;

        // Then reserve memory atomically with a CAS loop.
        let mut cur = self.used_memory.load(Ordering::Relaxed);
        loop {
            let next = cur.checked_add(memory_bytes)?;
            if next > self.memory_bytes {
                return None; // permit dropped here -> slot released
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

        // Threads.
        let mut cur_t = self.used_threads.load(Ordering::Relaxed);
        loop {
            let next = match cur_t.checked_add(threads) {
                Some(n) => n,
                None => {
                    self.used_memory.fetch_sub(memory_bytes, Ordering::AcqRel);
                    return None;
                }
            };
            if next > self.threads {
                self.used_memory.fetch_sub(memory_bytes, Ordering::AcqRel);
                return None;
            }
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

        // Process-wide governor: served work shares the hard machine cap with the
        // node's own (local) queries. If the governor refuses (machine cap or the
        // served-memory ceiling reached), undo this controller's accounting and
        // reject the admission so own + served work can't oversubscribe.
        let governor_lease = match &self.governor {
            Some(g) => match g.try_reserve(Role::Served, memory_bytes, threads) {
                Some(lease) => Some(lease),
                None => {
                    self.used_memory.fetch_sub(memory_bytes, Ordering::AcqRel);
                    self.used_threads.fetch_sub(threads, Ordering::AcqRel);
                    return None;
                }
            },
            None => None,
        };

        Some(Lease {
            controller: Arc::clone(self),
            memory_bytes,
            threads,
            _permit: permit,
            _governor_lease: governor_lease,
        })
    }
}

/// Free-resource snapshot advertised to requesters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FreeResources {
    pub memory_bytes: u64,
    pub threads: u32,
    pub free_jobs: u32,
}

/// A granted reservation; releases resources back to the budget on drop.
pub struct Lease {
    controller: Arc<AdmissionController>,
    memory_bytes: u64,
    threads: u32,
    _permit: OwnedSemaphorePermit,
    /// Released alongside this lease (returns the shared governor reservation).
    _governor_lease: Option<GovernorLease>,
}

impl Lease {
    pub fn memory_bytes(&self) -> u64 {
        self.memory_bytes
    }
    pub fn threads(&self) -> u32 {
        self.threads
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.controller
            .used_memory
            .fetch_sub(self.memory_bytes, Ordering::AcqRel);
        self.controller
            .used_threads
            .fetch_sub(self.threads, Ordering::AcqRel);
        // semaphore permit released automatically
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget() -> BudgetConfig {
        BudgetConfig {
            memory_bytes: 1000,
            threads: 4,
            max_jobs: 2,
            per_job_memory_bytes: 100,
            per_job_threads: 1,
            local_reserved_fraction: 0.0,
            data_classes: vec![DataClassCfg::Public],
        }
    }

    #[test]
    fn admits_until_job_slots_exhausted() {
        let ac = AdmissionController::new(&budget());
        let l1 = ac.try_admit(100, 1).unwrap();
        let l2 = ac.try_admit(100, 1).unwrap();
        // max_jobs = 2 -> third is refused even though mem/threads remain
        assert!(ac.try_admit(100, 1).is_none());
        drop(l1);
        assert!(ac.try_admit(100, 1).is_some());
        drop(l2);
    }

    #[test]
    fn refuses_when_memory_exceeded() {
        let ac = AdmissionController::new(&budget());
        let _l = ac.try_admit(900, 1).unwrap();
        // only 100 left, ask for 200
        assert!(ac.try_admit(200, 1).is_none());
    }

    #[test]
    fn lease_drop_restores_budget() {
        let ac = AdmissionController::new(&budget());
        {
            let _l = ac.try_admit(1000, 4).unwrap();
            assert_eq!(ac.free().memory_bytes, 0);
            assert_eq!(ac.free().threads, 0);
        }
        assert_eq!(ac.free().memory_bytes, 1000);
        assert_eq!(ac.free().threads, 4);
    }

    #[test]
    fn data_class_policy_enforced() {
        let ac = AdmissionController::new(&budget());
        assert!(ac.serves_data_class(DataClass::Public));
        assert!(!ac.serves_data_class(DataClass::Sensitive));
    }
}
