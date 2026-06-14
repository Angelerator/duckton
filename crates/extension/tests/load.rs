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
        .args(["-n", "duckdb_p2p"])
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
    let out_ext = tmp_dir.join("duckdb_p2p.duckdb_extension");
    assert!(build_loadable(&dylib, &platform, &out_ext), "metadata append failed");
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
    let (ok, stdout, _e) = run(&d2, "SELECT value FROM p2p_config() WHERE key = 'min_trust';");
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
    let (ok, _o, e) = run(&d3, "CALL p2p_economics(network => 'mainnet', confirm => true);");
    assert!(ok, "confirmed mainnet switch failed: {e}");
    let (_ok, stdout, _e) = run(&d3, "SELECT value FROM p2p_status() WHERE key = 'network';");
    assert_eq!(stdout, "mainnet", "confirmed switch should activate mainnet");

    // 4. Secrets are never echoed by p2p_config() nor written to the config file.
    let d4 = tmp_dir.join("c4");
    let _ = std::fs::remove_dir_all(&d4);
    let (ok, _o, e) = run(&d4, "CALL p2p_wallet(mnemonic => 'abandon abandon zoo secret words');");
    assert!(ok, "p2p_wallet failed: {e}");
    let (ok, stdout, _e) = run(
        &d4,
        "SELECT count(*) FROM p2p_config() WHERE value LIKE '%abandon%';",
    );
    assert!(ok);
    assert_eq!(stdout, "0", "secret must be redacted from p2p_config()");
    let runtime = std::fs::read_to_string(d4.join("runtime.toml")).unwrap_or_default();
    assert!(!runtime.contains("abandon"), "secret must not land in the config file");

    // 5. Anti-abuse deny-list: p2p_block persists, p2p_blocklist lists it, and
    //    p2p_unblock removes it (ARCHITECTURE "Abuse resistance").
    let d5 = tmp_dir.join("c5");
    let _ = std::fs::remove_dir_all(&d5);
    let (ok, _o, e) = run(&d5, "CALL p2p_block(id => 'b3:badactor', reason => 'cheating');");
    assert!(ok, "p2p_block failed: {e}");
    let (ok, stdout, _e) =
        run(&d5, "SELECT count(*) FROM p2p_blocklist() WHERE id = 'b3:badactor';");
    assert!(ok);
    assert_eq!(stdout, "1", "blocked actor must appear in p2p_blocklist()");
    // It is persisted to blocklist.toml under the hermetic config dir.
    let bl = std::fs::read_to_string(d5.join("blocklist.toml")).unwrap_or_default();
    assert!(bl.contains("b3:badactor"), "block must persist to blocklist.toml");
    // Unblock removes it.
    let (ok, _o, e) = run(&d5, "CALL p2p_unblock(id => 'b3:badactor');");
    assert!(ok, "p2p_unblock failed: {e}");
    let (ok, stdout, _e) =
        run(&d5, "SELECT count(*) FROM p2p_blocklist() WHERE id = 'b3:badactor';");
    assert!(ok);
    assert_eq!(stdout, "0", "unblocked actor must be gone from p2p_blocklist()");
}
