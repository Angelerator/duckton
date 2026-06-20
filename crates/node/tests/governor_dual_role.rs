//! Dual-role capacity-governor invariants (architecture §10).
//!
//! A node that is BOTH a requester running its own queries (`LocalExecutor`) AND
//! a worker serving others (`AdmissionController`) must not oversubscribe the
//! machine. These tests wire the SAME [`CapacityGovernor`] into both roles — as
//! `Node::with_config` / `Node::host_worker` do in production — and assert:
//!
//!  * own + served reservations together can never exceed the governor's
//!    memory / thread / job caps (the key can't-oversubscribe invariant);
//!  * served jobs can't starve local (the reserved headroom is always claimable);
//!  * local work can't starve serving (the layered `ram_fraction` budget leaves a
//!    share for serving);
//!  * `limits.worker_pool_size` is enforced as the global concurrent-job cap.

use std::sync::Arc;

use p2p_config::{BudgetConfig, DataClassCfg, PlannerConfig, PreferMode};
use p2p_node::{AdmissionController, CapacityGovernor, LocalExecutor, MockEngine, QueryEngine};

fn budget(memory_bytes: u64, threads: u32, max_jobs: u32, reserved: f64) -> BudgetConfig {
    BudgetConfig {
        memory_bytes,
        threads,
        max_jobs,
        per_job_memory_bytes: 100,
        per_job_threads: 1,
        local_reserved_fraction: reserved,
        data_classes: vec![DataClassCfg::Public],
    }
}

fn planner(ram_fraction: f64, max_local_jobs: usize) -> PlannerConfig {
    PlannerConfig {
        enabled: true,
        local_execution_enabled: true,
        prefer: PreferMode::Auto,
        ram_fraction,
        max_concurrent_local_jobs: max_local_jobs,
        size_threshold_bytes: u64::MAX,
        spill_tolerance_bytes: 0,
        max_local_latency_ms: 0,
    }
}

fn local_executor(
    b: &BudgetConfig,
    pc: &PlannerConfig,
    governor: Arc<CapacityGovernor>,
) -> Arc<LocalExecutor> {
    let engine = Arc::new(MockEngine::deterministic()) as Arc<dyn QueryEngine>;
    // Pass a 0 floor so test reservations are exact; the production path passes
    // `per_job_memory_bytes` (covered separately).
    LocalExecutor::governed(
        engine,
        b.memory_bytes,
        pc,
        b.per_job_threads,
        0,
        governor,
    )
}

#[test]
fn own_plus_served_memory_cannot_exceed_governor_cap() {
    // memory = 1000, no per-role reservation ⇒ the governor caps the SUM at 1000.
    let b = budget(1000, 8, 8, 0.0);
    let pc = planner(1.0, 8); // local budget == full 1000; governor is the cap
    let governor = CapacityGovernor::new(&b, b.max_jobs as usize, b.local_reserved_fraction, true);
    let local = local_executor(&b, &pc, Arc::clone(&governor));
    let admission = AdmissionController::governed(&b, Arc::clone(&governor));

    // Served takes 700, local takes 300 → exactly 1000 reserved across roles.
    let _served = admission.try_admit(700, 2).expect("served fits");
    let _own = local.reserve(300).expect("own fits the remaining 300");
    assert_eq!(governor.free_memory(), 0);

    // Neither role can reserve another byte — the machine cap is enforced for
    // BOTH the requester's own work and the jobs it serves.
    assert!(local.reserve(1).is_none(), "own work cannot oversubscribe");
    assert!(
        admission.try_admit(1, 0).is_none(),
        "served work cannot oversubscribe"
    );
}

#[test]
fn own_plus_served_threads_cannot_exceed_governor_cap() {
    let b = budget(1_000_000, 3, 8, 0.0);
    let pc = planner(1.0, 8);
    let governor = CapacityGovernor::new(&b, b.max_jobs as usize, b.local_reserved_fraction, true);
    let local = local_executor(&b, &pc, Arc::clone(&governor));
    let admission = AdmissionController::governed(&b, Arc::clone(&governor));

    let _served = admission.try_admit(1, 2).expect("served fits 2 threads");
    let _own = local.reserve(1).expect("own fits 1 thread"); // per_job_threads = 1
    assert_eq!(governor.free_threads(), 0);
    assert!(
        admission.try_admit(1, 1).is_none(),
        "no threads remain across roles"
    );
}

#[test]
fn served_jobs_cannot_starve_local() {
    // 20% of 1000 reserved for local ⇒ served may hold at most 800.
    let b = budget(1000, 8, 8, 0.2);
    let pc = planner(1.0, 8);
    let governor = CapacityGovernor::new(&b, b.max_jobs as usize, b.local_reserved_fraction, true);
    let local = local_executor(&b, &pc, Arc::clone(&governor));
    let admission = AdmissionController::governed(&b, Arc::clone(&governor));

    // Saturate the served pool to its 800 ceiling (2 × 400).
    let _s1 = admission.try_admit(400, 1).expect("served 400");
    let _s2 = admission.try_admit(400, 1).expect("served 800 total");
    // Served is at its ceiling: a third served job is refused even though 200 of
    // raw memory remains free…
    assert!(
        admission.try_admit(100, 1).is_none(),
        "served must not breach its ceiling"
    );
    // …because that 200 is reserved for the node's OWN work, which can still run.
    let _own = local.reserve(200).expect("reserved local headroom is claimable");
    assert_eq!(governor.free_memory(), 0);
}

#[test]
fn local_work_cannot_starve_serving() {
    // ram_fraction = 0.6 ⇒ the local budget is 600, so own work can never hold
    // more than 600 of the 1000-byte pool, leaving >= 400 for serving.
    let b = budget(1000, 8, 8, 0.0);
    let pc = planner(0.6, 8);
    let governor = CapacityGovernor::new(&b, b.max_jobs as usize, b.local_reserved_fraction, true);
    let local = local_executor(&b, &pc, Arc::clone(&governor));
    let admission = AdmissionController::governed(&b, Arc::clone(&governor));

    // Drive own work to its full local budget (600).
    let _o1 = local.reserve(300).expect("own 300");
    let _o2 = local.reserve(300).expect("own 600 total");
    // The local CAS now refuses more own work (budget exhausted)…
    assert!(local.reserve(1).is_none(), "own work bounded by ram_fraction budget");
    // …so serving still has its >= 400-byte share available.
    let _served = admission.try_admit(400, 1).expect("serving keeps its share");
    assert_eq!(governor.free_memory(), 0);
}

#[test]
fn worker_pool_size_caps_total_concurrent_jobs() {
    // Global job-slot cap (worker_pool_size) = 2, but memory/threads are ample
    // and each role's own slot limit is higher (4) — so the governor's global
    // slot semaphore is the binding constraint across BOTH roles.
    let b = budget(1_000_000, 64, 4, 0.0);
    let pc = planner(1.0, 4);
    let worker_pool_size = 2;
    let governor = CapacityGovernor::new(&b, worker_pool_size, b.local_reserved_fraction, true);
    let local = local_executor(&b, &pc, Arc::clone(&governor));
    let admission = AdmissionController::governed(&b, Arc::clone(&governor));

    let _own = local.reserve(1).expect("own takes slot 1");
    let _served = admission.try_admit(1, 1).expect("served takes slot 2");
    assert_eq!(governor.available_slots(), 0);

    // Both roles are blocked purely by the global slot cap (plenty of mem/threads).
    assert!(local.reserve(1).is_none(), "own blocked by worker_pool_size");
    assert!(
        admission.try_admit(1, 1).is_none(),
        "served blocked by worker_pool_size"
    );
}
