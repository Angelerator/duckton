//! Loadability scenario: build the extension cdylib, append the DuckDB metadata
//! footer, `LOAD` it into the real `duckdb` CLI, and query its table functions.
//!
//! This is skipped (not failed) when the `duckdb` CLI or `python3` is not on
//! PATH, so `cargo test` stays green in minimal environments — but when the CLI
//! is present (as in this dev environment) it genuinely exercises LOAD.

use std::path::PathBuf;
use std::process::Command;

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
    for name in ["libduckdb_p2p.dylib", "libduckdb_p2p.so", "duckdb_p2p.dll"] {
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
    let platform = String::from_utf8_lossy(&platform_out.stdout).trim().to_string();
    assert!(!platform.is_empty(), "could not determine duckdb platform");

    // Append the metadata footer to produce a loadable .duckdb_extension.
    // NOTE: DuckDB derives the init symbol from the file's basename, so it MUST
    // be named `duckdb_p2p.duckdb_extension` to match `duckdb_p2p_init_c_api`.
    let tmp_dir = std::env::temp_dir().join("p2p_ext_load_test");
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let out_ext = tmp_dir.join("duckdb_p2p.duckdb_extension");
    let script = repo_root().join("scripts/append_extension_metadata.py");
    let status = Command::new("python3")
        .arg(&script)
        .args(["-l", dylib.to_str().unwrap()])
        .args(["-n", "duckdb_p2p"])
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
