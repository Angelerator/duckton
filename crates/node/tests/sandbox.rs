//! OS-level execution sandbox tests (architecture §9.4).
//!
//! What is ACTUALLY EXERCISED on this macOS host:
//!  * Portable rlimit caps applied to a real child job process (file-size cap
//!    constrains/kills a runaway writer; fd/CPU caps are visible to the child).
//!  * macOS Seatbelt (`sandbox-exec`) enforcement: a sandboxed child can read a
//!    scoped fixture but is blocked from reading a disallowed path.
//!  * The egress allow-list + resolved spec wiring derived from `[storage]`.
//!
//! What is IMPLEMENTED-FOR-TARGET and unit-tested as pure builders (see the
//! `sandbox::linux` / `sandbox::windows` module tests): Linux cgroups v2 +
//! seccomp + nftables egress, Windows Job Objects + AppContainer + firewall.
//! Those require a Linux / Windows host to enforce and are not fabricated here.

use p2p_config::{SandboxConfig, SandboxEgressMode, StorageConfig};
use p2p_node::sandbox::{self, EgressAllowList, ResourceLimits, SandboxSpec};

fn spec_with_limits(limits: ResourceLimits) -> SandboxSpec {
    SandboxSpec {
        limits,
        egress: EgressAllowList::default(),
        read_only_paths: vec![],
        writable_paths: vec![std::env::temp_dir().display().to_string()],
        allow_network: false,
    }
}

#[test]
fn egress_allowlist_is_derived_from_storage_config() {
    // inherit_storage mode: the allow-list comes from the configured storage
    // endpoints + the enabled providers' well-known endpoints.
    let mut storage = StorageConfig::default();
    storage.enabled_providers = vec!["s3".into()];
    storage.endpoint = Some("https://minio.internal:9000".into());
    let cfg = SandboxConfig {
        enabled: true,
        ..SandboxConfig::default()
    };
    let egress = EgressAllowList::derive(&cfg, &storage);
    let auths: Vec<String> = egress.endpoints.iter().map(|e| e.to_authority()).collect();
    assert!(auths.contains(&"minio.internal:9000".to_string()));
    assert!(auths.contains(&"s3.amazonaws.com:443".to_string()));

    // explicit mode: only the explicit list (storage endpoints ignored).
    let cfg = SandboxConfig {
        enabled: true,
        egress_mode: SandboxEgressMode::Explicit,
        egress_allowlist: vec!["only.example:443".into()],
        ..SandboxConfig::default()
    };
    let egress = EgressAllowList::derive(&cfg, &storage);
    assert_eq!(egress.endpoints.len(), 1);
    assert_eq!(egress.endpoints[0].to_authority(), "only.example:443");
}

#[test]
fn disabled_sandbox_is_noop_and_runs_program() {
    let cfg = SandboxConfig {
        enabled: false,
        ..SandboxConfig::default()
    };
    let sb = sandbox::build(&cfg);
    assert_eq!(sb.name(), "noop");
    // A no-op sandbox must still be able to build a runnable command.
    #[cfg(unix)]
    {
        use std::ffi::OsString;
        let spec = spec_with_limits(ResourceLimits::default());
        let out = sb
            .command(&OsString::from("/bin/echo"), &[OsString::from("hi")], &spec)
            .unwrap()
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");
    }
}

// ---------------------------------------------------------------------------
// Portable rlimit enforcement — actually exercised on this macOS host.
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn rlimit_file_size_cap_constrains_a_runaway_writer() {
    use p2p_node::sandbox::{RlimitSandbox, Sandbox};
    use std::ffi::OsString;

    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("big.bin");
    let cap: u64 = 64 * 1024; // 64 KiB hard file-size cap

    let spec = spec_with_limits(ResourceLimits {
        max_file_size_bytes: Some(cap),
        // CPU backstop so the test can never hang even if the cap misbehaves.
        cpu_seconds: Some(10),
        ..ResourceLimits::default()
    });

    // dd tries to write 1 MiB; RLIMIT_FSIZE should stop it well before that
    // (SIGXFSZ / EFBIG). The limit is inherited by dd from the sh we cap.
    let script = format!(
        "dd if=/dev/zero of='{}' bs=1024 count=1024 2>/dev/null",
        out.display()
    );
    let status = RlimitSandbox
        .command(
            &OsString::from("/bin/sh"),
            &[OsString::from("-c"), OsString::from(script)],
            &spec,
        )
        .unwrap()
        .status()
        .unwrap();

    assert!(
        !status.success(),
        "writer should have been stopped by RLIMIT_FSIZE"
    );
    let written = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    assert!(
        written <= cap,
        "file grew to {written} bytes, exceeding the {cap}-byte cap"
    );
}

#[cfg(unix)]
#[test]
fn rlimit_fd_and_cpu_caps_are_applied_to_child() {
    use p2p_node::sandbox::{RlimitSandbox, Sandbox};
    use std::ffi::OsString;

    let spec = spec_with_limits(ResourceLimits {
        max_open_files: Some(48), // ulimit -n (count) — unambiguous unit
        cpu_seconds: Some(7),     // ulimit -t (seconds) — unambiguous unit
        ..ResourceLimits::default()
    });

    let out = RlimitSandbox
        .command(
            &OsString::from("/bin/sh"),
            &[OsString::from("-c"), OsString::from("ulimit -n; ulimit -t")],
            &spec,
        )
        .unwrap()
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let nums: Vec<&str> = text.split_whitespace().collect();
    assert_eq!(nums.len(), 2, "unexpected ulimit output: {text:?}");
    assert_eq!(nums[0], "48", "RLIMIT_NOFILE not applied (got {text:?})");
    assert_eq!(nums[1], "7", "RLIMIT_CPU not applied (got {text:?})");
}

// ---------------------------------------------------------------------------
// macOS Seatbelt enforcement — actually exercised when sandbox-exec is present.
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
#[test]
fn macos_seatbelt_blocks_disallowed_read_but_allows_scoped_read() {
    use p2p_node::sandbox::{MacosSandbox, Sandbox};
    use std::ffi::OsString;

    let sb = MacosSandbox::new();
    if !sb.seatbelt_available() {
        eprintln!("sandbox-exec not available on this host; skipping Seatbelt enforcement test");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let allowed = dir.path().join("ok.txt");
    std::fs::write(&allowed, "hello").unwrap();
    // Canonicalize so the allow-list subpath matches the real (/private/...) path.
    let scoped = std::fs::canonicalize(dir.path()).unwrap();

    let spec = SandboxSpec {
        limits: ResourceLimits::default(),
        egress: EgressAllowList::default(),
        read_only_paths: vec![scoped.display().to_string()],
        writable_paths: vec![scoped.display().to_string()],
        allow_network: false,
    };

    // Allowed: read a file inside the scoped fixture dir.
    let allowed_real = std::fs::canonicalize(&allowed).unwrap();
    let ok = sb
        .command(
            &OsString::from("/bin/cat"),
            &[OsString::from(allowed_real.as_os_str())],
            &spec,
        )
        .unwrap()
        .output()
        .unwrap();
    assert!(
        ok.status.success(),
        "scoped read should succeed; stderr={}",
        String::from_utf8_lossy(&ok.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&ok.stdout).trim(), "hello");

    // Disallowed: reading /etc/hosts is outside the allow-list ⇒ denied.
    let bad = sb
        .command(
            &OsString::from("/bin/cat"),
            &[OsString::from("/etc/hosts")],
            &spec,
        )
        .unwrap()
        .output()
        .unwrap();
    assert!(
        !bad.status.success(),
        "reading a disallowed path must be blocked by the Seatbelt profile"
    );
}
