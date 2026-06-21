//! Loadability scenario: build the extension cdylib, append the DuckDB metadata
//! footer, `LOAD` it into the real `duckdb` CLI, and query its table functions.
//!
//! This is skipped (not failed) when the `duckdb` CLI or `python3` is not on
//! PATH, so `cargo test` stays green in minimal environments — but when the CLI
//! is present (as in this dev environment) it genuinely exercises LOAD.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn have(cmd: &str, arg: &str) -> bool {
    Command::new(cmd)
        .arg(arg)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Locate the built cdylib next to the test binary (target/<profile>/).
fn locate_cdylib() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    // .../target/<profile>/deps/load-<hash>  -> .../target/<profile>/
    let profile_dir = exe.parent()?.parent()?;
    for name in ["libduckton.dylib", "libduckton.so", "duckton.dll"] {
        let p = profile_dir.join(name);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Resolve the DuckDB platform string (e.g. `osx_arm64`).
fn duckdb_platform() -> String {
    let out = Command::new("duckdb")
        .args(["-list", "-noheader", "-c", "PRAGMA platform;"])
        .output()
        .expect("run duckdb platform");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Append the DuckDB metadata footer to the cdylib, producing a loadable
/// `.duckdb_extension` at `out`. Returns false if the tooling failed.
fn build_loadable(dylib: &PathBuf, platform: &str, out: &PathBuf) -> bool {
    let script = repo_root().join("scripts/append_extension_metadata.py");
    Command::new("python3")
        .arg(&script)
        .args(["-l", dylib.to_str().unwrap()])
        .args(["-n", "duckton"])
        .args(["-p", platform])
        .args(["-dv", "v1.0.0"])
        .args(["-ev", "0.1.0"])
        .args(["-o", out.to_str().unwrap()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[test]
fn extension_loads_into_duckdb_and_runs_table_functions() {
    if !have("duckdb", "--version") || !have("python3", "--version") {
        eprintln!("SKIP: duckdb CLI and/or python3 not available");
        return;
    }
    let Some(dylib) = locate_cdylib() else {
        eprintln!("SKIP: built cdylib not found next to test binary");
        return;
    };

    // Resolve the DuckDB platform string (e.g. osx_arm64).
    let platform_out = Command::new("duckdb")
        .args(["-list", "-noheader", "-c", "PRAGMA platform;"])
        .output()
        .expect("run duckdb platform");
    let platform = String::from_utf8_lossy(&platform_out.stdout)
        .trim()
        .to_string();
    assert!(!platform.is_empty(), "could not determine duckdb platform");

    // Append the metadata footer to produce a loadable .duckdb_extension.
    // NOTE: DuckDB derives the init symbol from the file's basename, so it MUST
    // be named `duckton.duckdb_extension` to match `duckton_init_c_api`.
    let tmp_dir = std::env::temp_dir().join("p2p_ext_load_test");
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let out_ext = tmp_dir.join("duckton.duckdb_extension");
    let script = repo_root().join("scripts/append_extension_metadata.py");
    let status = Command::new("python3")
        .arg(&script)
        .args(["-l", dylib.to_str().unwrap()])
        .args(["-n", "duckton"])
        .args(["-p", &platform])
        .args(["-dv", "v1.0.0"])
        .args(["-ev", "0.1.0"])
        .args(["-o", out_ext.to_str().unwrap()])
        .status()
        .expect("run metadata script");
    assert!(status.success(), "metadata append failed");

    // LOAD and query p2p_info().
    let sql = format!(
        "LOAD '{}'; SELECT value FROM p2p_info() WHERE key='protocol_name';",
        out_ext.display()
    );
    let out = Command::new("duckdb")
        .args(["-unsigned", "-list", "-noheader", "-c", &sql])
        .output()
        .expect("run duckdb load");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "duckdb LOAD failed: stdout={stdout} stderr={stderr}"
    );
    assert!(
        stdout.contains("duckdb-p2p"),
        "p2p_info() did not return expected value; got: {stdout}"
    );

    // Config precedence through the extension: p2p_peers() reflects P2P_CONFIG.
    let cfg = std::env::temp_dir().join("duckdb_p2p_test_cfg.toml");
    std::fs::write(
        &cfg,
        "[discovery]\nbootstrap = [\"quic://seed-a:9494\"]\ncandidate_sample_size = 8\n",
    )
    .unwrap();
    let sql2 = format!(
        "LOAD '{}'; SELECT value FROM p2p_peers() WHERE kind='bootstrap';",
        out_ext.display()
    );
    let out2 = Command::new("duckdb")
        .args(["-unsigned", "-list", "-noheader", "-c", &sql2])
        .env("P2P_CONFIG", &cfg)
        .output()
        .expect("run duckdb peers");
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    assert!(out2.status.success(), "p2p_peers query failed: {stdout2}");
    assert!(
        stdout2.contains("quic://seed-a:9494"),
        "p2p_peers() did not reflect P2P_CONFIG; got: {stdout2}"
    );
}

/// The #1 audit ask: the distributed grid surface is reachable from a real
/// `LOAD`. `FROM p2p_query('SELECT 42 AS x')` drives the live `p2p-node`
/// coordinator down its **free local-execution** path (in-process locked-down
/// DuckDB) and streams the rows back through SQL. Hermetic config dir ⇒ no seeds
/// ⇒ local-first.
#[test]
fn extension_p2p_query_local_execution_end_to_end() {
    if !have("duckdb", "--version") || !have("python3", "--version") {
        eprintln!("SKIP: duckdb CLI and/or python3 not available");
        return;
    }
    let Some(dylib) = locate_cdylib() else {
        eprintln!("SKIP: built cdylib not found next to test binary");
        return;
    };
    let platform = duckdb_platform();
    assert!(!platform.is_empty(), "could not determine duckdb platform");

    let tmp_dir = std::env::temp_dir().join("p2p_ext_query_test");
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let out_ext = tmp_dir.join("duckton.duckdb_extension");
    assert!(
        build_loadable(&dylib, &platform, &out_ext),
        "metadata append failed"
    );
    let ext = out_ext.display().to_string();
    let cfg_dir = tmp_dir.join("cfg");
    let _ = std::fs::remove_dir_all(&cfg_dir);

    let run = |sql: &str| -> (bool, String, String) {
        let full = format!("LOAD '{ext}'; {sql}");
        let out = Command::new("duckdb")
            .args(["-unsigned", "-list", "-noheader", "-c", &full])
            .env("P2P_CONFIG_DIR", &cfg_dir)
            .output()
            .expect("run duckdb p2p_query");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        )
    };

    // Single scalar: rows come back through SQL from the grid's local path.
    let (ok, stdout, stderr) = run("FROM p2p_query('SELECT 42 AS x');");
    assert!(ok, "p2p_query failed: {stderr}");
    assert_eq!(
        stdout, "42",
        "p2p_query did not return the row; got: {stdout}"
    );

    // A multi-row query materializes every row (exercises chunked emission).
    let (ok, stdout, stderr) = run("SELECT count(*) FROM p2p_query('SELECT * FROM range(5000)');");
    assert!(ok, "p2p_query range failed: {stderr}");
    assert_eq!(stdout, "5000", "p2p_query lost rows; got: {stdout}");

    // Grid path: pinning `prefer => 'remote'` with no reachable hosts surfaces
    // the friendly NoCandidates error (proving dispatch is wired, not faked).
    let (ok, _o, stderr) = run("FROM p2p_query('SELECT 1', prefer => 'remote');");
    assert!(!ok, "remote with no hosts should fail");
    assert!(
        stderr.to_lowercase().contains("no hosts available"),
        "expected NoCandidates message; got: {stderr}"
    );
}

/// P0-1: `p2p_query_meta` surfaces the `QueryOutcome` execution/verification
/// metadata (which `p2p_query` discards) back to SQL. A local-path query must
/// report `executed_locally=true`, `verified=true`, and the real result row
/// count — proving the introspection companion is wired to the live node.
#[test]
fn extension_p2p_query_meta_surfaces_outcome_metadata() {
    if !have("duckdb", "--version") || !have("python3", "--version") {
        eprintln!("SKIP: duckdb CLI and/or python3 not available");
        return;
    }
    let Some(dylib) = locate_cdylib() else {
        eprintln!("SKIP: built cdylib not found next to test binary");
        return;
    };
    let platform = duckdb_platform();
    assert!(!platform.is_empty(), "could not determine duckdb platform");

    let tmp_dir = std::env::temp_dir().join("p2p_ext_query_meta_test");
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let out_ext = tmp_dir.join("duckton.duckdb_extension");
    assert!(
        build_loadable(&dylib, &platform, &out_ext),
        "metadata append failed"
    );
    let ext = out_ext.display().to_string();
    let cfg_dir = tmp_dir.join("cfg");
    let _ = std::fs::remove_dir_all(&cfg_dir);

    let run = |sql: &str| -> (bool, String, String) {
        let full = format!("LOAD '{ext}'; {sql}");
        let out = Command::new("duckdb")
            .args(["-unsigned", "-list", "-noheader", "-c", &full])
            .env("P2P_CONFIG_DIR", &cfg_dir)
            .output()
            .expect("run duckdb p2p_query_meta");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        )
    };

    // The local (zero-grid) path runs in-process: executed_locally + verified.
    let (ok, stdout, stderr) =
        run("SELECT value FROM p2p_query_meta('SELECT 42 AS x') WHERE key='executed_locally';");
    assert!(ok, "p2p_query_meta failed: {stderr}");
    assert_eq!(
        stdout, "true",
        "local query must report executed_locally=true"
    );

    let (ok, stdout, _e) =
        run("SELECT value FROM p2p_query_meta('SELECT 42 AS x') WHERE key='verified';");
    assert!(ok);
    assert_eq!(stdout, "true", "own-machine local result is verified");

    let (ok, stdout, _e) =
        run("SELECT value FROM p2p_query_meta('SELECT * FROM range(3)') WHERE key='result_rows';");
    assert!(ok);
    assert_eq!(
        stdout, "3",
        "metadata must report the real result row count"
    );

    // p2p_query (the row surface) is unchanged: still returns only the rows.
    let (ok, stdout, _e) = run("FROM p2p_query('SELECT 42 AS x');");
    assert!(ok);
    assert_eq!(stdout, "42", "p2p_query row shape is unchanged");
}

/// `p2p_node_metadata()` surfaces this node's non-GDPR `SystemProfile` (machine
/// class: CPU/RAM/disk/OS, donated budget, resource ceilings) as (group, key,
/// value) rows. A requester-only node with no persisted profile collects one on
/// demand, so the table function must return real machine-class rows.
#[test]
fn extension_p2p_node_metadata_returns_rows() {
    if !have("duckdb", "--version") || !have("python3", "--version") {
        eprintln!("SKIP: duckdb CLI and/or python3 not available");
        return;
    }
    let Some(dylib) = locate_cdylib() else {
        eprintln!("SKIP: built cdylib not found next to test binary");
        return;
    };
    let platform = duckdb_platform();
    assert!(!platform.is_empty(), "could not determine duckdb platform");

    let tmp_dir = std::env::temp_dir().join("p2p_ext_node_metadata_test");
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let out_ext = tmp_dir.join("duckton.duckdb_extension");
    assert!(
        build_loadable(&dylib, &platform, &out_ext),
        "metadata append failed"
    );
    let ext = out_ext.display().to_string();
    let cfg_dir = tmp_dir.join("cfg");
    let _ = std::fs::remove_dir_all(&cfg_dir);

    let run = |sql: &str| -> (bool, String, String) {
        let full = format!("LOAD '{ext}'; {sql}");
        let out = Command::new("duckdb")
            .args(["-unsigned", "-list", "-noheader", "-c", &full])
            .env("P2P_CONFIG_DIR", &cfg_dir)
            .output()
            .expect("run duckdb p2p_node_metadata");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        )
    };

    // The table function returns a non-trivial set of machine-class rows.
    let (ok, stdout, stderr) = run("SELECT count(*) FROM p2p_node_metadata();");
    assert!(ok, "p2p_node_metadata failed: {stderr}");
    let n: u64 = stdout.parse().unwrap_or(0);
    assert!(n > 10, "expected many metadata rows, got {n}");

    // The CPU arch is detected and non-empty (a real machine-class field).
    let (ok, stdout, _e) = run("SELECT value FROM p2p_node_metadata() WHERE key = 'arch';");
    assert!(ok);
    assert!(!stdout.is_empty(), "cpu arch should be detected");

    // The donated budget is surfaced (distinct from physical RAM); with a
    // hermetic zero-config dir it is the default 4 GiB donation.
    let (ok, stdout, _e) =
        run("SELECT value FROM p2p_node_metadata() WHERE key = 'donated_mem_bytes';");
    assert!(ok);
    assert_eq!(
        stdout,
        (4u64 * 1024 * 1024 * 1024).to_string(),
        "donated budget should be the default 4 GiB"
    );
}

/// The free **local-execution** engine (`HostEngine`) must be locked down exactly
/// like the node's strict engine: no network egress and NO local-filesystem
/// access. A `p2p_query` that tries to read a local file off the free path must
/// FAIL — otherwise an untrusted query could exfiltrate `/etc/passwd` et al.
#[test]
fn extension_local_path_blocks_local_file_reads() {
    if !have("duckdb", "--version") || !have("python3", "--version") {
        eprintln!("SKIP: duckdb CLI and/or python3 not available");
        return;
    }
    let Some(dylib) = locate_cdylib() else {
        eprintln!("SKIP: built cdylib not found next to test binary");
        return;
    };
    let platform = duckdb_platform();
    assert!(!platform.is_empty(), "could not determine duckdb platform");

    let tmp_dir = std::env::temp_dir().join("p2p_ext_lockdown_test");
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let out_ext = tmp_dir.join("duckton.duckdb_extension");
    assert!(
        build_loadable(&dylib, &platform, &out_ext),
        "metadata append failed"
    );
    let ext = out_ext.display().to_string();
    let cfg_dir = tmp_dir.join("cfg");
    let _ = std::fs::remove_dir_all(&cfg_dir);

    // Plant a secret file the sandboxed query must NOT be able to read.
    let secret = tmp_dir.join("secret.csv");
    std::fs::write(&secret, "col\ntopsecret\n").unwrap();
    let secret_path = secret.display().to_string().replace('\'', "''");

    let run = |sql: &str| -> (bool, String, String) {
        let full = format!("LOAD '{ext}'; {sql}");
        let out = Command::new("duckdb")
            .args(["-unsigned", "-list", "-noheader", "-c", &full])
            .env("P2P_CONFIG_DIR", &cfg_dir)
            .output()
            .expect("run duckdb lockdown");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        )
    };

    // Sanity: a pure in-memory query still works on the free local path.
    let (ok, stdout, stderr) = run("FROM p2p_query('SELECT 1 AS x');");
    assert!(ok, "in-memory local query should still work: {stderr}");
    assert_eq!(stdout, "1");

    // Reading a local file off the free local path must be BLOCKED (disabled
    // filesystem / external access off), and must NOT leak the file contents.
    let (ok, stdout, stderr) = run(&format!(
        "FROM p2p_query('SELECT * FROM read_csv_auto(''{secret_path}'')');"
    ));
    assert!(
        !ok,
        "local file read should be blocked on the free local path; stdout={stdout}"
    );
    assert!(
        !stdout.contains("topsecret") && !stderr.contains("topsecret"),
        "secret file contents must not leak; stdout={stdout} stderr={stderr}"
    );
}

/// Defense-in-depth on the production `HostEngine`/`p2p_query` path: every
/// filesystem-escape / exfiltration primitive must ERROR under the strict
/// lockdown (no `enable_external_access`, `disabled_filesystems`, locked config,
/// ephemeral temp). Mirrors `docker/scenarios/13_sandbox.sh`. None of these may
/// succeed and none may leak the planted secret. Runs against the real DuckDB
/// CLI (skipped when it / python3 is unavailable).
#[test]
fn extension_host_engine_denies_fs_escapes() {
    if !have("duckdb", "--version") || !have("python3", "--version") {
        eprintln!("SKIP: duckdb CLI and/or python3 not available");
        return;
    }
    let Some(dylib) = locate_cdylib() else {
        eprintln!("SKIP: built cdylib not found next to test binary");
        return;
    };
    let platform = duckdb_platform();
    assert!(!platform.is_empty(), "could not determine duckdb platform");

    let tmp_dir = std::env::temp_dir().join("p2p_ext_fsescape_test");
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let out_ext = tmp_dir.join("duckton.duckdb_extension");
    assert!(
        build_loadable(&dylib, &platform, &out_ext),
        "metadata append failed"
    );
    let ext = out_ext.display().to_string();
    let cfg_dir = tmp_dir.join("cfg");
    let _ = std::fs::remove_dir_all(&cfg_dir);

    // Plant a secret file + an output target the sandboxed query must NOT touch.
    let secret = tmp_dir.join("secret.csv");
    std::fs::write(&secret, "col\ntopsecret\n").unwrap();
    let secret_path = secret.display().to_string().replace('\'', "''");
    let out_csv = tmp_dir.join("exfil.csv");
    let _ = std::fs::remove_file(&out_csv);
    let out_path = out_csv.display().to_string().replace('\'', "''");

    let run = |sql: &str| -> (bool, String, String) {
        let full = format!("LOAD '{ext}'; {sql}");
        let out = Command::new("duckdb")
            .args(["-unsigned", "-list", "-noheader", "-c", &full])
            .env("P2P_CONFIG_DIR", &cfg_dir)
            .output()
            .expect("run duckdb fs-escape");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        )
    };

    // The inner single quotes must be doubled for the outer p2p_query('...').
    let denied: Vec<String> = vec![
        format!("COPY (SELECT 1) TO ''{out_path}''"),
        "ATTACH ''attack.db''".to_string(),
        "EXPORT DATABASE ''/tmp/p2p_ext_export''".to_string(),
        format!("SELECT * FROM read_text(''{secret_path}'')"),
        format!("SELECT * FROM read_csv_auto(''{secret_path}'')"),
        "SELECT count(*) FROM glob(''/**'')".to_string(),
    ];
    for inner in &denied {
        let (ok, stdout, stderr) = run(&format!("FROM p2p_query('{inner}');"));
        assert!(
            !ok,
            "host-engine escape must be blocked: `{inner}` succeeded; stdout={stdout}"
        );
        assert!(
            !stdout.contains("topsecret") && !stderr.contains("topsecret"),
            "secret must not leak via `{inner}`; stdout={stdout} stderr={stderr}"
        );
    }
    // The COPY target must not have been created.
    assert!(
        !out_csv.exists(),
        "COPY TO must not have written outside the sandbox"
    );

    // Sanity: a pure in-memory query still works on the locked-down path.
    let (ok, stdout, stderr) = run("FROM p2p_query('SELECT 7 AS x');");
    assert!(ok, "in-memory query should still work: {stderr}");
    assert_eq!(stdout, "7");
}

/// The host/swarm surface is callable from SQL and drives the live node:
/// `p2p_join` persists seeds + rebuilds discovery; `p2p_share` persists the
/// donated budget and spawns the worker accept loop (the node binds a real
/// listen address). Hermetic config dir.
#[test]
fn extension_p2p_share_and_join_callable() {
    if !have("duckdb", "--version") || !have("python3", "--version") {
        eprintln!("SKIP: duckdb CLI and/or python3 not available");
        return;
    }
    let Some(dylib) = locate_cdylib() else {
        eprintln!("SKIP: built cdylib not found next to test binary");
        return;
    };
    let platform = duckdb_platform();
    assert!(!platform.is_empty(), "could not determine duckdb platform");

    let tmp_dir = std::env::temp_dir().join("p2p_ext_grid_test");
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let out_ext = tmp_dir.join("duckton.duckdb_extension");
    assert!(
        build_loadable(&dylib, &platform, &out_ext),
        "metadata append failed"
    );
    let ext = out_ext.display().to_string();

    let run = |cfg_dir: &PathBuf, sql: &str| -> (bool, String, String) {
        let full = format!("LOAD '{ext}'; {sql}");
        let out = Command::new("duckdb")
            .args(["-unsigned", "-list", "-noheader", "-c", &full])
            .env("P2P_CONFIG_DIR", cfg_dir)
            .output()
            .expect("run duckdb grid");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        )
    };

    // p2p_join persists the seed and reports it back from the live node.
    let dj = tmp_dir.join("join");
    let _ = std::fs::remove_dir_all(&dj);
    let (ok, stdout, stderr) = run(
        &dj,
        "SELECT value FROM p2p_join(bootstrap => ['quic://127.0.0.1:9494']) WHERE key='bootstrap';",
    );
    assert!(ok, "p2p_join failed: {stderr}");
    assert_eq!(
        stdout, "quic://127.0.0.1:9494",
        "join did not reflect seed; got: {stdout}"
    );
    // It persisted to the runtime layer.
    let runtime = std::fs::read_to_string(dj.join("runtime.toml")).unwrap_or_default();
    assert!(
        runtime.contains("9494"),
        "seed must persist to runtime.toml; got: {runtime}"
    );

    // p2p_share makes the node a host: it binds a real listen address and
    // reports the donated budget.
    let ds = tmp_dir.join("share");
    let _ = std::fs::remove_dir_all(&ds);
    let (ok, stdout, stderr) = run(
        &ds,
        "SELECT value FROM p2p_share(memory => '2GB', threads => 2, max_jobs => 3, \
         data_classes => ['public']) WHERE key='memory_bytes';",
    );
    assert!(ok, "p2p_share failed: {stderr}");
    assert_eq!(
        stdout,
        (2u64 << 30).to_string(),
        "share budget not applied; got: {stdout}"
    );
    let (ok, stdout, _e) = run(&ds, "SELECT value FROM p2p_share() WHERE key='status';");
    assert!(ok);
    assert_eq!(stdout, "hosting", "node should be hosting after p2p_share");
}

/// Full distributed grid over SQL across **two real processes**: one `duckdb`
/// process becomes a host (`p2p_share`, bound to a fixed loopback port) and
/// stays alive; a second process `p2p_join`s it and runs
/// `FROM p2p_query(..., prefer => 'remote')`, which dispatches the query to the
/// host over QUIC, runs it there, and streams the verified result back — all
/// driven from SQL. Proves the grid path, not just local execution.
#[test]
fn extension_two_node_grid_query_over_sql() {
    if !have("duckdb", "--version") || !have("python3", "--version") {
        eprintln!("SKIP: duckdb CLI and/or python3 not available");
        return;
    }
    let Some(dylib) = locate_cdylib() else {
        eprintln!("SKIP: built cdylib not found next to test binary");
        return;
    };
    let platform = duckdb_platform();
    assert!(!platform.is_empty(), "could not determine duckdb platform");

    let tmp_dir = std::env::temp_dir().join("p2p_ext_two_node_test");
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let out_ext = tmp_dir.join("duckton.duckdb_extension");
    assert!(
        build_loadable(&dylib, &platform, &out_ext),
        "metadata append failed"
    );
    let ext = out_ext.display().to_string();

    let host_dir = tmp_dir.join("host");
    let req_dir = tmp_dir.join("req");
    let _ = std::fs::remove_dir_all(&host_dir);
    let _ = std::fs::remove_dir_all(&req_dir);
    let port = 28494; // fixed loopback port so the requester knows where to dial

    // Spawn the host: LOAD + p2p_share, then keep its stdin open so the process
    // (and its worker accept loop) stays alive while we query it.
    let mut host = Command::new("duckdb")
        .arg("-unsigned")
        .env("P2P_BIND_ADDR", format!("127.0.0.1:{port}"))
        .env("P2P_CONFIG_DIR", &host_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn host duckdb");
    let mut host_stdin = host.stdin.take().expect("host stdin");
    writeln!(
        host_stdin,
        "LOAD '{ext}'; CALL p2p_share(memory => '1GB', threads => 1, max_jobs => 2);"
    )
    .expect("write host SQL");
    host_stdin.flush().ok();

    // Give the host a moment to bind its endpoint and start serving, then query
    // it from a separate requester process (a few attempts to absorb startup).
    let req_sql = format!(
        "LOAD '{ext}'; CALL p2p_join(bootstrap => ['quic://127.0.0.1:{port}']); \
         SELECT x FROM p2p_query('SELECT 42 AS x', prefer => 'remote', replicas => 1, \
         quorum => 1, min_trust => 0.0);"
    );
    let mut got = String::new();
    let mut last_err = String::new();
    for attempt in 0..5 {
        std::thread::sleep(std::time::Duration::from_millis(if attempt == 0 {
            1500
        } else {
            800
        }));
        let out = Command::new("duckdb")
            .args(["-unsigned", "-list", "-noheader", "-c", &req_sql])
            .env("P2P_CONFIG_DIR", &req_dir)
            .output()
            .expect("run requester");
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        last_err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if out.status.success() && stdout.contains("42") {
            got = stdout;
            break;
        }
    }

    // Tear the host down regardless of outcome.
    drop(host_stdin);
    let _ = host.kill();
    let _ = host.wait();

    assert!(
        got.contains("42"),
        "two-node grid query over SQL did not return the remote result; last stderr: {last_err}"
    );
}

/// End-to-end SQL admin/config surface through the loaded extension: zero-config
/// defaults, friendly setters, the mainnet safety guard, and secret redaction.
/// Hermetic: all state lives under a temp `P2P_CONFIG_DIR`.
#[test]
fn extension_sql_admin_surface_end_to_end() {
    if !have("duckdb", "--version") || !have("python3", "--version") {
        eprintln!("SKIP: duckdb CLI and/or python3 not available");
        return;
    }
    let Some(dylib) = locate_cdylib() else {
        eprintln!("SKIP: built cdylib not found next to test binary");
        return;
    };
    let platform = duckdb_platform();
    assert!(!platform.is_empty(), "could not determine duckdb platform");

    let tmp_dir = std::env::temp_dir().join("p2p_ext_admin_test");
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let out_ext = tmp_dir.join("duckton.duckdb_extension");
    assert!(
        build_loadable(&dylib, &platform, &out_ext),
        "metadata append failed"
    );
    let ext = out_ext.display().to_string();

    // Each scenario gets a fresh, isolated config dir so nothing leaks to $HOME.
    let run = |cfg_dir: &PathBuf, sql: &str| -> (bool, String, String) {
        let full = format!("LOAD '{ext}'; {sql}");
        let out = Command::new("duckdb")
            .args(["-unsigned", "-list", "-noheader", "-c", &full])
            .env("P2P_CONFIG_DIR", cfg_dir)
            .output()
            .expect("run duckdb admin");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        )
    };

    // 1. Zero-config default: the active network is testnet (the safe default).
    let d1 = tmp_dir.join("c1");
    let (ok, stdout, stderr) = run(&d1, "SELECT value FROM p2p_status() WHERE key = 'network';");
    assert!(ok, "p2p_status failed: {stderr}");
    assert_eq!(stdout, "testnet", "default network should be testnet");

    // 2. A friendly setter persists and is reflected in p2p_config().
    let d2 = tmp_dir.join("c2");
    let _ = std::fs::remove_dir_all(&d2);
    let (ok, _o, e) = run(&d2, "CALL p2p_trust(min_trust => 0.83);");
    assert!(ok, "p2p_trust setter failed: {e}");
    let (ok, stdout, _e) = run(
        &d2,
        "SELECT value FROM p2p_config() WHERE key = 'min_trust';",
    );
    assert!(ok);
    assert_eq!(stdout, "0.83", "setter must persist + be reflected");

    // 3. Mainnet safety guard: switching to mainnet WITHOUT confirm must fail.
    let d3 = tmp_dir.join("c3");
    let _ = std::fs::remove_dir_all(&d3);
    let (ok, _o, stderr) = run(&d3, "CALL p2p_economics(network => 'mainnet');");
    assert!(!ok, "mainnet switch without confirm must fail");
    assert!(
        stderr.to_lowercase().contains("real ton"),
        "guard message should warn about real TON; got: {stderr}"
    );
    // ...and WITH confirm it switches.
    let (ok, _o, e) = run(
        &d3,
        "CALL p2p_economics(network => 'mainnet', confirm => true);",
    );
    assert!(ok, "confirmed mainnet switch failed: {e}");
    let (_ok, stdout, _e) = run(&d3, "SELECT value FROM p2p_status() WHERE key = 'network';");
    assert_eq!(
        stdout, "mainnet",
        "confirmed switch should activate mainnet"
    );

    // 4. Secrets are never echoed by p2p_config() nor written to the config file.
    let d4 = tmp_dir.join("c4");
    let _ = std::fs::remove_dir_all(&d4);
    let (ok, _o, e) = run(
        &d4,
        "CALL p2p_wallet(mnemonic => 'abandon abandon zoo secret words');",
    );
    assert!(ok, "p2p_wallet failed: {e}");
    let (ok, stdout, _e) = run(
        &d4,
        "SELECT count(*) FROM p2p_config() WHERE value LIKE '%abandon%';",
    );
    assert!(ok);
    assert_eq!(stdout, "0", "secret must be redacted from p2p_config()");
    let runtime = std::fs::read_to_string(d4.join("runtime.toml")).unwrap_or_default();
    assert!(
        !runtime.contains("abandon"),
        "secret must not land in the config file"
    );

    // 5. Anti-abuse deny-list: p2p_block persists, p2p_blocklist lists it, and
    //    p2p_unblock removes it (ARCHITECTURE "Abuse resistance").
    let d5 = tmp_dir.join("c5");
    let _ = std::fs::remove_dir_all(&d5);
    let (ok, _o, e) = run(
        &d5,
        "CALL p2p_block(id => 'b3:badactor', reason => 'cheating');",
    );
    assert!(ok, "p2p_block failed: {e}");
    let (ok, stdout, _e) = run(
        &d5,
        "SELECT count(*) FROM p2p_blocklist() WHERE id = 'b3:badactor';",
    );
    assert!(ok);
    assert_eq!(stdout, "1", "blocked actor must appear in p2p_blocklist()");
    // It is persisted to blocklist.toml under the hermetic config dir.
    let bl = std::fs::read_to_string(d5.join("blocklist.toml")).unwrap_or_default();
    assert!(
        bl.contains("b3:badactor"),
        "block must persist to blocklist.toml"
    );
    // Unblock removes it.
    let (ok, _o, e) = run(&d5, "CALL p2p_unblock(id => 'b3:badactor');");
    assert!(ok, "p2p_unblock failed: {e}");
    let (ok, stdout, _e) = run(
        &d5,
        "SELECT count(*) FROM p2p_blocklist() WHERE id = 'b3:badactor';",
    );
    assert!(ok);
    assert_eq!(
        stdout, "0",
        "unblocked actor must be gone from p2p_blocklist()"
    );
}
