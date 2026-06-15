//! macOS Seatbelt (`sandbox-exec`) profile generation for the OS sandbox
//! (architecture §9.4).
//!
//! Generates a TinyScheme Seatbelt profile (`.sb` syntax) that:
//!  * denies everything by default (`(deny default)`),
//!  * imports the platform base profile (`(import "system.sb")`) so a normal
//!    dynamically-linked binary can launch (dyld / shared cache / mach basics)
//!    *without* widening the filesystem read allow-list,
//!  * allows reads of the scoped fixture directories ([`SandboxSpec::read_only_paths`]),
//!  * allows reads+writes only under the job scratch/temp dir ([`SandboxSpec::writable_paths`]),
//!  * denies all network egress unless [`SandboxSpec::allow_network`], in which
//!    case it allows outbound TCP only to the configured storage endpoints'
//!    ports (Seatbelt matches on `remote tcp "*:<port>"`; hostname-level
//!    filtering is not expressible in Seatbelt, so the egress *port* set is
//!    enforced at this layer and the *host* set is enforced by the scoped,
//!    short-lived credentials + the Linux nftables layer — see module docs).
//!
//! Verified on this host (macOS 15): the generated profile launches `/bin/cat`,
//! permits a read inside the scoped dir, and blocks a read of `/etc/hosts`.
//!
//! `sandbox-exec` is deprecated-but-present on current macOS; we resolve its
//! path and gracefully fall back to plain rlimits when it is absent.

use std::collections::BTreeSet;
use std::path::PathBuf;

use super::SandboxSpec;

/// Locate `sandbox-exec` on this host (canonical path first, then `PATH`).
pub fn find_sandbox_exec() -> Option<PathBuf> {
    let canonical = PathBuf::from("/usr/bin/sandbox-exec");
    if canonical.exists() {
        return Some(canonical);
    }
    // Fall back to a PATH scan.
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let cand = PathBuf::from(dir).join("sandbox-exec");
            if cand.exists() {
                return Some(cand);
            }
        }
    }
    None
}

/// Escape a path for embedding in a Seatbelt `(literal "...")`/`(subpath "...")`.
fn sb_str(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Build the Seatbelt profile string for a resolved [`SandboxSpec`].
pub fn seatbelt_profile(spec: &SandboxSpec) -> String {
    let mut p = String::new();
    p.push_str("(version 1)\n");
    p.push_str("(deny default)\n");
    // Import the platform base profile: lets a dynamically-linked binary launch
    // (dyld, the dyld shared cache, mach bootstrap, sysctl-read, ...) WITHOUT
    // widening the filesystem read allow-list — `(deny default)` still governs
    // arbitrary file reads, so /etc/passwd & friends stay blocked.
    p.push_str("(import \"system.sb\")\n");
    p.push_str("(allow process-fork)\n");
    p.push_str("(allow process-exec)\n");

    // Scoped fixture reads (the configured local data dirs).
    for ro in &spec.read_only_paths {
        let ro = ro.trim();
        if !ro.is_empty() {
            p.push_str(&format!(
                "(allow file-read* (subpath \"{}\"))\n",
                sb_str(ro)
            ));
        }
    }

    // Writable scratch/temp dirs (read + write).
    for w in &spec.writable_paths {
        let w = w.trim();
        if !w.is_empty() {
            p.push_str(&format!("(allow file-read* (subpath \"{}\"))\n", sb_str(w)));
            p.push_str(&format!(
                "(allow file-write* (subpath \"{}\"))\n",
                sb_str(w)
            ));
        }
    }

    // Network egress.
    if spec.allow_network && !spec.egress.is_empty() {
        // Seatbelt cannot match outbound by hostname; allow the union of the
        // configured destination ports (host scoping is provided by the scoped
        // credentials + the Linux nftables layer). DNS is required to resolve.
        let mut ports: BTreeSet<u16> = BTreeSet::new();
        for ep in &spec.egress.endpoints {
            if let Some(port) = ep.port {
                ports.insert(port);
            }
        }
        // Allow DNS resolution.
        p.push_str("(allow network-outbound (remote udp \"*:53\"))\n");
        p.push_str("(allow network-outbound (remote tcp \"*:53\"))\n");
        if ports.is_empty() {
            // No explicit ports — fall back to HTTPS only.
            p.push_str("(allow network-outbound (remote tcp \"*:443\"))\n");
        } else {
            for port in ports {
                p.push_str(&format!(
                    "(allow network-outbound (remote tcp \"*:{port}\"))\n"
                ));
            }
        }
    }
    // else: network stays denied by the default rule.

    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::{EgressAllowList, EgressEndpoint, ResourceLimits, SandboxSpec};

    fn spec(allow_network: bool, ports: &[u16]) -> SandboxSpec {
        SandboxSpec {
            limits: ResourceLimits::default(),
            egress: EgressAllowList {
                endpoints: ports
                    .iter()
                    .map(|p| EgressEndpoint::new("s3.amazonaws.com", Some(*p)))
                    .collect(),
            },
            read_only_paths: vec!["/srv/fixtures".into()],
            writable_paths: vec!["/tmp/job-123".into()],
            allow_network,
        }
    }

    #[test]
    fn profile_denies_by_default_and_scopes_fs() {
        let p = seatbelt_profile(&spec(false, &[]));
        assert!(p.starts_with("(version 1)\n(deny default)\n(import \"system.sb\")\n"));
        assert!(p.contains("(allow file-read* (subpath \"/srv/fixtures\"))"));
        assert!(p.contains("(allow file-write* (subpath \"/tmp/job-123\"))"));
        // Fixture dir is read-only (no write rule for it).
        assert!(!p.contains("(allow file-write* (subpath \"/srv/fixtures\"))"));
    }

    #[test]
    fn profile_denies_network_when_not_allowed() {
        let p = seatbelt_profile(&spec(false, &[]));
        assert!(!p.contains("network-outbound"));
    }

    #[test]
    fn profile_allows_only_configured_egress_ports() {
        let p = seatbelt_profile(&spec(true, &[443, 9000]));
        assert!(p.contains("(allow network-outbound (remote tcp \"*:443\"))"));
        assert!(p.contains("(allow network-outbound (remote tcp \"*:9000\"))"));
        // DNS is permitted so hostnames resolve.
        assert!(p.contains("(remote udp \"*:53\")"));
        // A non-configured port is not present.
        assert!(!p.contains("\"*:21\""));
    }

    #[test]
    fn profile_escapes_quotes_in_paths() {
        let mut s = spec(false, &[]);
        s.read_only_paths = vec!["/weird/\"path".into()];
        let p = seatbelt_profile(&s);
        assert!(p.contains("/weird/\\\"path"));
    }
}
