//! `p2p-job-exec` — the sandboxed per-job child executor (architecture §9.4,
//! G1/G8). The host spawns ONE of these per job (wrapped by the OS sandbox via
//! `Sandbox::command`) when `[sandbox].process_per_job` is enabled; it reads a
//! single `JobRequest` from stdin, runs it on the locked-down `DuckDbEngine`, and
//! writes the `JobResponse` to stdout. The OS limits (rlimits / Seatbelt /
//! cgroups / Job Object) are applied by the parent's sandbox wrapper, so a job
//! that exceeds its lease is killed without affecting the node.
//!
//! Build (needs a C/C++ toolchain for the bundled DuckDB):
//!   `cargo build -p p2p-node --features duckdb-engine --bin p2p-job-exec`
//! then point the host at it with `P2P_JOB_EXEC=/path/to/p2p-job-exec` and enable
//! `[sandbox].process_per_job` (+ a real `[sandbox].backend`).

#[cfg(feature = "duckdb-engine")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    use p2p_node::{DuckDbEngine, QueryEngine};

    // Resolve the storage profile (allowed dirs / remote access) from the same
    // layered config the host uses, so the child reads exactly what the policy
    // permits. A build error here means the engine cannot be constructed.
    let cfg = p2p_config::GridConfig::load(None)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    let engine: Box<dyn QueryEngine> = match DuckDbEngine::from_storage_config(&cfg.storage) {
        Ok(e) => Box::new(e),
        Err(e) => {
            // Surface the failure as a JobResponse so the parent gets a clean
            // error rather than an empty pipe.
            eprintln!("p2p-job-exec: engine init failed: {e}");
            std::process::exit(3);
        }
    };

    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    p2p_node::serve_job(engine.as_ref(), &mut stdin, &mut stdout).await
}

#[cfg(not(feature = "duckdb-engine"))]
fn main() {
    eprintln!(
        "p2p-job-exec requires the `duckdb-engine` feature (the bundled DuckDB engine). \
         Rebuild with: cargo build -p p2p-node --features duckdb-engine --bin p2p-job-exec"
    );
    std::process::exit(2);
}
