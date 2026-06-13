//! OS-level execution sandbox wrapped AROUND job execution (architecture §9.4).
//!
//! This is the deferred §9.4 control and the documented **required complement**
//! to the secure cloud-read story: DuckDB's own lockdown
//! (`enable_external_access`, `lock_configuration`, `allowed_directories`,
//! ephemeral temp — see [`crate::duckdb_engine`]) hardens the *engine*, but
//! DuckDB cannot **scope network egress** to specific storage endpoints
//! (`enable_external_access` is all-or-nothing) and cannot impose a *hard* RAM /
//! CPU / file-descriptor / file-size ceiling that survives a hostile query. The
//! operating system must. This module adds that boundary.
//!
//! ## Pluggable, trait-per-collaborator
//! [`Sandbox`] mirrors the project's existing trait pattern ([`QueryEngine`],
//! [`StorageProvider`], `Discovery`, `TrustStore`). Backends:
//!  * [`NoopSandbox`] — default, today's behavior (tests / unsupported hosts).
//!  * [`RlimitSandbox`] — portable POSIX `setrlimit` caps (RAM/CPU/FD/file-size)
//!    applied to a child job process. Works on this macOS host.
//!  * [`MacosSandbox`] — a Seatbelt (`sandbox-exec`) profile restricting the
//!    filesystem (read-only to the scoped fixtures/temp) and network egress to
//!    the configured storage endpoints, *plus* the rlimit caps.
//!  * [`LinuxSandbox`] — cgroups v2 (memory/CPU caps) + seccomp (syscall
//!    filtering) + network-egress allow-list (nftables), *plus* rlimits. The
//!    policy builders are implemented and unit-tested here; **enforcement
//!    requires a Linux host** (it cannot be exercised on this macOS machine).
//!
//! ## Enforcement model — honest scope
//! A *hard* per-job cap that can **kill a runaway query without taking down the
//! node** requires running the job in its own **child process**: that is the
//! [`Sandbox::command`] API (rlimits via `pre_exec`, Seatbelt/cgroups/seccomp/
//! egress installed for that child). The current engine runs DuckDB *in-process*
//! via `spawn_blocking`; per-job rlimits cannot be applied to a single thread of
//! a shared process without affecting sibling jobs, so [`Sandbox::enter_job`]
//! (the in-process hook the worker calls) records/telemeters the resolved policy
//! and is a no-op for the shared-process model. The genuinely OS-enforced path
//! is [`Sandbox::command`], which the tests exercise (a child exceeding a
//! configured cap is constrained/killed on this macOS host).
//!
//! Everything is driven by [`p2p_config::SandboxConfig`] (the `[sandbox]`
//! section) and the `[storage]` endpoints — nothing is hard-coded. Default
//! `enabled = false` ⇒ [`NoopSandbox`] ⇒ behavior is unchanged.

use std::ffi::{OsStr, OsString};
use std::process::Command;
use std::sync::Arc;

use p2p_config::{
    SandboxBackend, SandboxConfig, SandboxEgressMode, SandboxLimitsMode, StorageConfig,
};

pub mod linux;
pub mod macos;
pub mod windows;

/// Errors from the sandbox layer.
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("sandbox backend {0:?} is not supported on this platform")]
    Unsupported(SandboxBackend),
    #[error("failed to apply resource limit {resource}: {source}")]
    Rlimit {
        resource: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("sandbox setup failed: {0}")]
    Setup(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

// ---------------------------------------------------------------------------
// Resolved policy: limits + egress + filesystem scope
// ---------------------------------------------------------------------------

/// The donated per-job budget the sandbox limits are matched against in
/// `inherit_budget` mode (architecture §10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobBudget {
    pub memory_bytes: u64,
    pub threads: u32,
}

/// Resolved per-resource OS limits (`None` = no cap installed for that
/// resource). Derived from [`SandboxConfig`] + the per-job [`JobBudget`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResourceLimits {
    /// Address-space / RAM cap (`RLIMIT_AS`), bytes.
    pub memory_bytes: Option<u64>,
    /// CPU-time cap (`RLIMIT_CPU`), seconds.
    pub cpu_seconds: Option<u64>,
    /// Maximum created-file size (`RLIMIT_FSIZE`), bytes.
    pub max_file_size_bytes: Option<u64>,
    /// Open file-descriptor cap (`RLIMIT_NOFILE`), count.
    pub max_open_files: Option<u64>,
    /// Process/thread cap (`RLIMIT_NPROC`), count.
    pub max_processes: Option<u64>,
}

impl ResourceLimits {
    /// Resolve the effective caps from the `[sandbox]` config and the per-job
    /// budget. In `inherit_budget` mode RAM comes from the lease and the process
    /// cap is derived from the donated threads (with headroom); CPU / file-size
    /// / fd caps have no budget analogue so the explicit fields are honored when
    /// non-zero. In `explicit` mode every cap comes from config (`0` = no cap).
    pub fn resolve(cfg: &SandboxConfig, budget: JobBudget) -> Self {
        let l = &cfg.limits;
        let nz = |v: u64| (v > 0).then_some(v);
        match l.mode {
            SandboxLimitsMode::InheritBudget => Self {
                memory_bytes: nz(budget.memory_bytes),
                cpu_seconds: nz(l.cpu_seconds),
                max_file_size_bytes: nz(l.max_file_size_bytes),
                max_open_files: nz(l.max_open_files),
                // Allow the donated threads plus a small fixed headroom for
                // runtime/helper threads so a legitimate parallel query is not
                // throttled, while still bounding fork bombs.
                max_processes: (budget.threads > 0).then(|| budget.threads as u64 + 8),
            },
            SandboxLimitsMode::Explicit => Self {
                memory_bytes: nz(l.memory_bytes),
                cpu_seconds: nz(l.cpu_seconds),
                max_file_size_bytes: nz(l.max_file_size_bytes),
                max_open_files: nz(l.max_open_files),
                max_processes: nz(l.max_processes),
            },
        }
    }
}

/// One allowed network destination for the egress filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressEndpoint {
    /// Hostname or IP (a leading `*.` denotes a wildcard suffix match).
    pub host: String,
    /// Optional TCP port; `None` ⇒ any port for this host.
    pub port: Option<u16>,
}

impl EgressEndpoint {
    pub fn new(host: impl Into<String>, port: Option<u16>) -> Self {
        Self {
            host: host.into(),
            port,
        }
    }

    /// Render as `host` or `host:port`.
    pub fn to_authority(&self) -> String {
        match self.port {
            Some(p) => format!("{}:{}", self.host, p),
            None => self.host.clone(),
        }
    }
}

/// The network egress allow-list: a job may reach these destinations and
/// nothing else. Empty ⇒ all network egress denied.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EgressAllowList {
    pub endpoints: Vec<EgressEndpoint>,
}

impl EgressAllowList {
    pub fn is_empty(&self) -> bool {
        self.endpoints.is_empty()
    }

    /// Derive the allow-list from the `[sandbox]` egress mode and the
    /// `[storage]` providers/endpoints. In `inherit_storage` mode the configured
    /// storage endpoints (and the well-known endpoints of each enabled cloud
    /// provider) are collected, then the explicit `egress_allowlist` is
    /// appended. In `explicit` mode only the explicit list is used. The result
    /// is de-duplicated, order-preserving.
    pub fn derive(cfg: &SandboxConfig, storage: &StorageConfig) -> Self {
        let mut out: Vec<EgressEndpoint> = Vec::new();
        let mut push = |ep: Option<EgressEndpoint>| {
            if let Some(ep) = ep {
                if !out.contains(&ep) {
                    out.push(ep);
                }
            }
        };

        if matches!(cfg.egress_mode, SandboxEgressMode::InheritStorage) {
            // Explicitly configured storage endpoint.
            push(parse_endpoint(storage.endpoint.as_deref().unwrap_or("")));
            // Per-provider endpoint overrides.
            for opts in storage.provider_options.values() {
                if let Some(ep) = opts.get("endpoint") {
                    push(parse_endpoint(ep));
                }
            }
            // Well-known public endpoints for each enabled provider.
            for prov in &storage.enabled_providers {
                for ep in well_known_endpoints(prov, storage.region.as_deref()) {
                    push(Some(ep));
                }
            }
        }

        // Explicit entries (appended in inherit mode, sole source in explicit).
        for e in &cfg.egress_allowlist {
            push(parse_endpoint(e));
        }

        Self { endpoints: out }
    }
}

/// Parse a `host`, `host:port`, or `scheme://host:port/...` string into an
/// [`EgressEndpoint`]. Returns `None` for empty/garbage input.
pub fn parse_endpoint(s: &str) -> Option<EgressEndpoint> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // Drop scheme + path/query, keep the authority.
    let authority = s
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(s)
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("");
    // Strip any userinfo.
    let authority = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
    if authority.is_empty() {
        return None;
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => match port.parse::<u16>() {
            Ok(p) => Some(EgressEndpoint::new(host, Some(p))),
            // A colon with a non-numeric tail (rare) — treat the whole thing as host.
            Err(_) => Some(EgressEndpoint::new(authority, None)),
        },
        _ => Some(EgressEndpoint::new(authority, None)),
    }
}

/// Well-known public TCP endpoints for an enabled storage provider id, used to
/// seed the egress allow-list in `inherit_storage` mode. Region-specific S3
/// hosts are added when a region is configured.
fn well_known_endpoints(provider: &str, region: Option<&str>) -> Vec<EgressEndpoint> {
    match provider {
        "s3" => {
            let mut v = vec![EgressEndpoint::new("s3.amazonaws.com", Some(443))];
            if let Some(r) = region.map(str::trim).filter(|r| !r.is_empty()) {
                v.push(EgressEndpoint::new(format!("s3.{r}.amazonaws.com"), Some(443)));
            }
            v
        }
        "az" | "azure" | "abfss" => {
            // Wildcard suffix: <account>.blob.core.windows.net / dfs...
            vec![
                EgressEndpoint::new("*.blob.core.windows.net", Some(443)),
                EgressEndpoint::new("*.dfs.core.windows.net", Some(443)),
            ]
        }
        "gcs" | "gs" => vec![EgressEndpoint::new("storage.googleapis.com", Some(443))],
        // Generic HTTPS / local-fake have no implied endpoint.
        _ => vec![],
    }
}

/// The fully-resolved sandbox policy for one job: OS resource caps, the network
/// egress allow-list, and the filesystem scope (read-only fixtures + the
/// writable scratch/temp dir).
#[derive(Debug, Clone)]
pub struct SandboxSpec {
    pub limits: ResourceLimits,
    pub egress: EgressAllowList,
    /// Directories the job may read (the configured local fixtures).
    pub read_only_paths: Vec<String>,
    /// Directories the job may write (scratch/temp).
    pub writable_paths: Vec<String>,
    /// Whether any network egress is permitted at all. When `false` the OS
    /// profile denies all network; when `true` egress is restricted to
    /// [`SandboxSpec::egress`].
    pub allow_network: bool,
}

impl SandboxSpec {
    /// Build the resolved spec from config + storage + the per-job budget +
    /// the writable temp dir for this job.
    pub fn resolve(
        cfg: &SandboxConfig,
        storage: &StorageConfig,
        budget: JobBudget,
        temp_dir: impl Into<String>,
    ) -> Self {
        let egress = EgressAllowList::derive(cfg, storage);
        // Network is permitted only when storage remote access is on AND there
        // is at least one allowed destination (otherwise deny everything).
        let allow_network = storage.enable_remote_access && !egress.is_empty();
        Self {
            limits: ResourceLimits::resolve(cfg, budget),
            egress,
            read_only_paths: storage.allowed_local_paths.clone(),
            writable_paths: vec![temp_dir.into()],
            allow_network,
        }
    }
}

// ---------------------------------------------------------------------------
// The Sandbox trait + guard
// ---------------------------------------------------------------------------

/// Reverts any reversible in-process changes on drop. For the shared-process
/// model this is intentionally a no-op (see the module docs); it exists so the
/// worker can hold a guard for the job's lifetime and so a future
/// process-per-job executor can attach cleanup here.
#[derive(Default)]
pub struct JobGuard {
    _private: (),
}

impl JobGuard {
    pub fn noop() -> Self {
        Self::default()
    }
}

/// A pluggable OS-level execution sandbox (architecture §9.4).
pub trait Sandbox: Send + Sync {
    /// The effective backend this instance implements.
    fn backend(&self) -> SandboxBackend;

    /// Human-readable backend name (for logs/telemetry).
    fn name(&self) -> &'static str;

    /// In-process hook the worker calls around the (in-process) engine
    /// execution. Returns a guard held for the job's lifetime. For the
    /// shared-process model this records the policy but does not mutate global
    /// process limits (which would affect sibling jobs); the OS-enforced path is
    /// [`Sandbox::command`]. The default is a no-op.
    fn enter_job(&self, _spec: &SandboxSpec) -> Result<JobGuard, SandboxError> {
        Ok(JobGuard::noop())
    }

    /// Build a child [`Command`] that runs `program` with `args` fully wrapped
    /// by this OS sandbox (the genuinely enforced, process-per-job path):
    /// rlimits via `pre_exec`, plus Seatbelt / cgroups / seccomp / egress where
    /// the backend supports it. This is what the tests exercise.
    fn command(
        &self,
        program: &OsStr,
        args: &[OsString],
        spec: &SandboxSpec,
    ) -> Result<Command, SandboxError>;
}

/// Build the configured sandbox. Resolves `auto` to the best backend for the
/// current platform and degrades gracefully to [`NoopSandbox`] anywhere a
/// requested backend is not available on this target. `enabled = false` always
/// yields [`NoopSandbox`]. The returned trait object is platform-independent for
/// callers; the concrete backend is chosen here behind `#[cfg]`.
pub fn build(cfg: &SandboxConfig) -> Arc<dyn Sandbox> {
    if !cfg.enabled {
        return Arc::new(NoopSandbox);
    }
    match effective_backend(cfg.backend) {
        SandboxBackend::None => Arc::new(NoopSandbox),
        SandboxBackend::Rlimit => make_unix_rlimit(),
        SandboxBackend::MacosSeatbelt => make_macos(),
        // cgroups+seccomp and the Android profile both use the Linux backend.
        SandboxBackend::CgroupsSeccomp | SandboxBackend::Android => make_linux(),
        SandboxBackend::WindowsJobObject => make_windows(),
        SandboxBackend::Ios => Arc::new(IosSandbox),
        // `Auto` is resolved away by `effective_backend`.
        SandboxBackend::Auto => Arc::new(NoopSandbox),
    }
}

fn make_unix_rlimit() -> Arc<dyn Sandbox> {
    #[cfg(unix)]
    {
        Arc::new(RlimitSandbox)
    }
    #[cfg(not(unix))]
    {
        Arc::new(NoopSandbox)
    }
}

fn make_macos() -> Arc<dyn Sandbox> {
    #[cfg(unix)]
    {
        Arc::new(MacosSandbox::new())
    }
    #[cfg(not(unix))]
    {
        Arc::new(NoopSandbox)
    }
}

fn make_linux() -> Arc<dyn Sandbox> {
    #[cfg(unix)]
    {
        Arc::new(LinuxSandbox::new())
    }
    #[cfg(not(unix))]
    {
        Arc::new(NoopSandbox)
    }
}

fn make_windows() -> Arc<dyn Sandbox> {
    #[cfg(windows)]
    {
        Arc::new(WindowsSandbox::new())
    }
    #[cfg(not(windows))]
    {
        Arc::new(NoopSandbox)
    }
}

/// Resolve `auto` to a concrete backend for the host platform. Pure function of
/// the compile target, so it is unit-tested per platform.
pub fn effective_backend(b: SandboxBackend) -> SandboxBackend {
    match b {
        SandboxBackend::Auto => {
            if cfg!(target_os = "android") {
                SandboxBackend::Android
            } else if cfg!(target_os = "ios") {
                SandboxBackend::Ios
            } else if cfg!(target_os = "linux") {
                SandboxBackend::CgroupsSeccomp
            } else if cfg!(target_os = "macos") {
                SandboxBackend::MacosSeatbelt
            } else if cfg!(target_os = "windows") {
                SandboxBackend::WindowsJobObject
            } else if cfg!(unix) {
                // Other Unixes (BSD, etc.) get the portable rlimit caps.
                SandboxBackend::Rlimit
            } else {
                SandboxBackend::None
            }
        }
        other => other,
    }
}

// ---------------------------------------------------------------------------
// NoopSandbox
// ---------------------------------------------------------------------------

/// The default sandbox: applies nothing. Identical to today's behavior; used
/// for tests and unsupported platforms.
pub struct NoopSandbox;

impl Sandbox for NoopSandbox {
    fn backend(&self) -> SandboxBackend {
        SandboxBackend::None
    }
    fn name(&self) -> &'static str {
        "noop"
    }
    fn command(
        &self,
        program: &OsStr,
        args: &[OsString],
        _spec: &SandboxSpec,
    ) -> Result<Command, SandboxError> {
        let mut cmd = Command::new(program);
        cmd.args(args);
        Ok(cmd)
    }
}

// ---------------------------------------------------------------------------
// rlimit (portable, Unix) — setrlimit via pre_exec
// ---------------------------------------------------------------------------

/// Portable resource-limit sandbox: caps RAM (`RLIMIT_AS`), CPU
/// (`RLIMIT_CPU`), created-file size (`RLIMIT_FSIZE`), open files
/// (`RLIMIT_NOFILE`) and processes (`RLIMIT_NPROC`) on the child job process via
/// a `pre_exec` hook. Unix only (Linux/macOS/BSD/Android). A real OS cap
/// complementing DuckDB's own `memory_limit`/`threads`. (There is no `rlimit` on
/// Windows — see [`WindowsSandbox`].)
#[cfg(unix)]
pub struct RlimitSandbox;

#[cfg(unix)]
impl Sandbox for RlimitSandbox {
    fn backend(&self) -> SandboxBackend {
        SandboxBackend::Rlimit
    }
    fn name(&self) -> &'static str {
        "rlimit"
    }
    fn command(
        &self,
        program: &OsStr,
        args: &[OsString],
        spec: &SandboxSpec,
    ) -> Result<Command, SandboxError> {
        let mut cmd = Command::new(program);
        cmd.args(args);
        apply_rlimits_pre_exec(&mut cmd, &spec.limits);
        Ok(cmd)
    }
}

/// `(RLIMIT_* constant, value)` pairs to install, in a platform-correct type.
#[cfg(all(unix, target_os = "linux"))]
type RawResource = libc::__rlimit_resource_t;
#[cfg(all(unix, not(target_os = "linux")))]
type RawResource = libc::c_int;

/// Translate the resolved [`ResourceLimits`] into raw `(resource, value)` pairs.
#[cfg(unix)]
fn rlimit_settings(limits: &ResourceLimits) -> Vec<(RawResource, u64)> {
    let mut v: Vec<(RawResource, u64)> = Vec::new();
    if let Some(m) = limits.memory_bytes {
        v.push((libc::RLIMIT_AS, m));
    }
    if let Some(c) = limits.cpu_seconds {
        v.push((libc::RLIMIT_CPU, c));
    }
    if let Some(f) = limits.max_file_size_bytes {
        v.push((libc::RLIMIT_FSIZE, f));
    }
    if let Some(n) = limits.max_open_files {
        v.push((libc::RLIMIT_NOFILE, n));
    }
    if let Some(p) = limits.max_processes {
        v.push((libc::RLIMIT_NPROC, p));
    }
    v
}

/// Attach a `pre_exec` closure (runs in the forked child, before `exec`) that
/// lowers the configured rlimits. Only ever *lowers* the soft limit (never
/// raises the hard limit), so it needs no privilege. Async-signal-safe: only
/// `getrlimit`/`setrlimit` syscalls.
#[cfg(unix)]
fn apply_rlimits_pre_exec(cmd: &mut Command, limits: &ResourceLimits) {
    use std::os::unix::process::CommandExt;
    let settings = rlimit_settings(limits);
    if settings.is_empty() {
        return;
    }
    // SAFETY: the closure only calls async-signal-safe syscalls and touches no
    // heap allocation beyond the pre-built `settings` vector it captures.
    unsafe {
        cmd.pre_exec(move || {
            for (res, val) in &settings {
                let mut cur: libc::rlimit = std::mem::zeroed();
                if libc::getrlimit(*res, &mut cur) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let want = *val as libc::rlim_t;
                // Never raise the hard limit (avoids EPERM); clamp to it.
                let new_cur = if cur.rlim_max == libc::RLIM_INFINITY {
                    want
                } else {
                    want.min(cur.rlim_max)
                };
                let rl = libc::rlimit {
                    rlim_cur: new_cur,
                    rlim_max: cur.rlim_max,
                };
                if libc::setrlimit(*res, &rl) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}

// ---------------------------------------------------------------------------
// macOS — Seatbelt (sandbox-exec) + rlimits
// ---------------------------------------------------------------------------

/// macOS sandbox: wraps the child in `sandbox-exec` with a generated Seatbelt
/// profile (filesystem read-only to the scoped fixtures/temp, network egress
/// restricted to the configured storage endpoints) and also applies the rlimit
/// caps. Falls back to plain rlimits if `sandbox-exec` is unavailable. Unix-only
/// (it composes the rlimit `pre_exec` path).
#[cfg(unix)]
pub struct MacosSandbox {
    /// Resolved path to `sandbox-exec` (if present on this host).
    sandbox_exec: Option<std::path::PathBuf>,
}

#[cfg(unix)]
impl Default for MacosSandbox {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(unix)]
impl MacosSandbox {
    pub fn new() -> Self {
        Self {
            sandbox_exec: macos::find_sandbox_exec(),
        }
    }

    /// Whether `sandbox-exec` is available to actually enforce the profile.
    pub fn seatbelt_available(&self) -> bool {
        self.sandbox_exec.is_some()
    }
}

#[cfg(unix)]
impl Sandbox for MacosSandbox {
    fn backend(&self) -> SandboxBackend {
        SandboxBackend::MacosSeatbelt
    }
    fn name(&self) -> &'static str {
        "macos-seatbelt"
    }
    fn command(
        &self,
        program: &OsStr,
        args: &[OsString],
        spec: &SandboxSpec,
    ) -> Result<Command, SandboxError> {
        let mut cmd = match &self.sandbox_exec {
            Some(se) => {
                let profile = macos::seatbelt_profile(spec);
                let mut c = Command::new(se);
                c.arg("-p").arg(profile).arg("--").arg(program).args(args);
                c
            }
            None => {
                // No Seatbelt on this host — still apply the portable rlimits.
                let mut c = Command::new(program);
                c.args(args);
                c
            }
        };
        apply_rlimits_pre_exec(&mut cmd, &spec.limits);
        Ok(cmd)
    }
}

// ---------------------------------------------------------------------------
// Linux — cgroups v2 + seccomp + egress (policy builders here; enforcement
// requires a Linux host)
// ---------------------------------------------------------------------------

/// Linux sandbox: composes cgroups v2 (memory/CPU caps), seccomp-bpf (syscall
/// filtering) and an nftables network-egress allow-list, plus rlimits. The
/// policy builders ([`linux::CgroupV2Policy`], [`linux::SeccompPolicy`],
/// [`linux::nftables_egress_rules`]) are implemented and unit-tested here;
/// **actually installing** a cgroup / seccomp filter / nftables ruleset
/// requires a Linux host (and usually elevated privilege), so on non-Linux
/// hosts `command` applies only the portable rlimits.
///
/// Also used for the **Android** backend: an Android node runs inside the
/// platform app sandbox + SELinux already; this adds seccomp-bpf + app-scoped
/// cgroup limits where available. An Android node is a constrained host.
#[cfg(unix)]
pub struct LinuxSandbox {
    _private: (),
}

#[cfg(unix)]
impl Default for LinuxSandbox {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(unix)]
impl LinuxSandbox {
    pub fn new() -> Self {
        Self { _private: () }
    }
}

#[cfg(unix)]
impl Sandbox for LinuxSandbox {
    fn backend(&self) -> SandboxBackend {
        SandboxBackend::CgroupsSeccomp
    }
    fn name(&self) -> &'static str {
        "linux-cgroups-seccomp"
    }
    fn command(
        &self,
        program: &OsStr,
        args: &[OsString],
        spec: &SandboxSpec,
    ) -> Result<Command, SandboxError> {
        let mut cmd = Command::new(program);
        cmd.args(args);
        // rlimits are portable and always applied.
        apply_rlimits_pre_exec(&mut cmd, &spec.limits);
        // cgroup placement + seccomp installation are Linux/Android-only and
        // elided on other hosts; see the `linux` module for the policy builders
        // + how a Linux executor would wire them via pre_exec / a cgroup writer.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            linux::attach_seccomp_pre_exec(&mut cmd, spec);
        }
        Ok(cmd)
    }
}

// ---------------------------------------------------------------------------
// iOS — OS app sandbox only; no subprocesses
// ---------------------------------------------------------------------------

/// iOS sandbox. iOS apps are already strongly OS-sandboxed and **cannot spawn
/// subprocesses**, with strict background-execution limits. There is therefore
/// no child-process enforcement path: [`Sandbox::command`] returns
/// [`SandboxError::Unsupported`] and the node relies on the OS app sandbox plus
/// in-process resource accounting (DuckDB's own `memory_limit`/`threads` + the
/// admission controller). Realistically an iOS node acts as a **client /
/// requester or a light in-process host**, not a general multi-job compute host.
/// Compiles on every target (no platform APIs); only selected on iOS.
pub struct IosSandbox;

impl Sandbox for IosSandbox {
    fn backend(&self) -> SandboxBackend {
        SandboxBackend::Ios
    }
    fn name(&self) -> &'static str {
        "ios-app-sandbox"
    }
    fn command(
        &self,
        _program: &OsStr,
        _args: &[OsString],
        _spec: &SandboxSpec,
    ) -> Result<Command, SandboxError> {
        Err(SandboxError::Unsupported(SandboxBackend::Ios))
    }
}

// ---------------------------------------------------------------------------
// Windows — Job Objects + restricted token/AppContainer + WFP/firewall egress
// ---------------------------------------------------------------------------

/// Windows sandbox. Windows has no POSIX `rlimit`; resource caps are enforced
/// with a **Job Object** ([`windows::JobObjectLimits`], memory/CPU/active-process
/// caps with kill-on-close), filesystem + privilege isolation with a
/// **restricted token / AppContainer** ([`windows::WindowsIsolationPolicy`]),
/// and egress with **WFP / firewall** rules ([`windows::firewall_egress_rules`]).
///
/// Because a Job Object is applied to a process *after* creation (Windows has no
/// `fork`/`pre_exec`), [`Sandbox::command`] returns the bare command; the Windows
/// executor spawns it suspended, calls
/// [`windows::create_configured_job_object`] + [`windows::assign_process_to_job`],
/// then resumes it (closing the job handle reaps survivors via kill-on-close).
/// Compiled only on Windows; the policy builders are unit-tested on every host.
#[cfg(windows)]
pub struct WindowsSandbox {
    _private: (),
}

#[cfg(windows)]
impl Default for WindowsSandbox {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(windows)]
impl WindowsSandbox {
    pub fn new() -> Self {
        Self { _private: () }
    }

    /// The Job Object resource caps this sandbox would install for `spec`.
    pub fn job_object_limits(&self, spec: &SandboxSpec) -> windows::JobObjectLimits {
        windows::JobObjectLimits::from_spec(spec)
    }
}

#[cfg(windows)]
impl Sandbox for WindowsSandbox {
    fn backend(&self) -> SandboxBackend {
        SandboxBackend::WindowsJobObject
    }
    fn name(&self) -> &'static str {
        "windows-jobobject"
    }
    fn command(
        &self,
        program: &OsStr,
        args: &[OsString],
        _spec: &SandboxSpec,
    ) -> Result<Command, SandboxError> {
        // The bare command; Job Object assignment happens post-spawn (see the
        // type docs). Resource caps are installed by the executor via
        // `windows::create_configured_job_object` + `assign_process_to_job`.
        let mut cmd = Command::new(program);
        cmd.args(args);
        Ok(cmd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p2p_config::{SandboxLimitsConfig, SandboxLimitsMode};
    use std::collections::BTreeMap;

    fn sandbox_cfg() -> SandboxConfig {
        SandboxConfig {
            enabled: true,
            ..SandboxConfig::default()
        }
    }

    #[test]
    fn limits_inherit_budget() {
        let mut cfg = sandbox_cfg();
        cfg.limits = SandboxLimitsConfig {
            mode: SandboxLimitsMode::InheritBudget,
            max_file_size_bytes: 4096,
            ..SandboxLimitsConfig::default()
        };
        let l = ResourceLimits::resolve(
            &cfg,
            JobBudget {
                memory_bytes: 512 * 1024 * 1024,
                threads: 2,
            },
        );
        assert_eq!(l.memory_bytes, Some(512 * 1024 * 1024));
        assert_eq!(l.max_processes, Some(10)); // threads(2) + 8 headroom
        assert_eq!(l.max_file_size_bytes, Some(4096)); // honored even in inherit mode
        assert_eq!(l.cpu_seconds, None);
    }

    #[test]
    fn limits_explicit_zero_means_no_cap() {
        let mut cfg = sandbox_cfg();
        cfg.limits = SandboxLimitsConfig {
            mode: SandboxLimitsMode::Explicit,
            memory_bytes: 1024,
            cpu_seconds: 0,
            max_file_size_bytes: 2048,
            max_open_files: 0,
            max_processes: 0,
        };
        let l = ResourceLimits::resolve(
            &cfg,
            JobBudget {
                memory_bytes: 9,
                threads: 9,
            },
        );
        assert_eq!(l.memory_bytes, Some(1024));
        assert_eq!(l.max_file_size_bytes, Some(2048));
        assert_eq!(l.cpu_seconds, None);
        assert_eq!(l.max_open_files, None);
        assert_eq!(l.max_processes, None);
    }

    #[test]
    fn parse_endpoint_variants() {
        assert_eq!(
            parse_endpoint("https://s3.amazonaws.com:443/bucket/key"),
            Some(EgressEndpoint::new("s3.amazonaws.com", Some(443)))
        );
        assert_eq!(
            parse_endpoint("minio.internal"),
            Some(EgressEndpoint::new("minio.internal", None))
        );
        assert_eq!(
            parse_endpoint("user@host.example:9000"),
            Some(EgressEndpoint::new("host.example", Some(9000)))
        );
        assert_eq!(parse_endpoint("   "), None);
    }

    #[test]
    fn egress_inherits_storage_endpoints_and_providers() {
        let mut storage = StorageConfig::default();
        storage.enabled_providers = vec!["s3".into(), "gcs".into()];
        storage.region = Some("eu-west-1".into());
        storage.endpoint = Some("http://minio.local:9000".into());
        let mut s3opts = BTreeMap::new();
        s3opts.insert("endpoint".to_string(), "s3.custom.example:443".to_string());
        storage.provider_options.insert("s3".into(), s3opts);

        let cfg = sandbox_cfg(); // inherit_storage
        let egress = EgressAllowList::derive(&cfg, &storage);
        let auth: Vec<String> = egress.endpoints.iter().map(|e| e.to_authority()).collect();
        assert!(auth.contains(&"minio.local:9000".to_string()));
        assert!(auth.contains(&"s3.custom.example:443".to_string()));
        assert!(auth.contains(&"s3.amazonaws.com:443".to_string()));
        assert!(auth.contains(&"s3.eu-west-1.amazonaws.com:443".to_string()));
        assert!(auth.contains(&"storage.googleapis.com:443".to_string()));
    }

    #[test]
    fn egress_explicit_mode_ignores_storage() {
        let storage = StorageConfig::default(); // local-fake, has no endpoints
        let mut cfg = sandbox_cfg();
        cfg.egress_mode = SandboxEgressMode::Explicit;
        cfg.egress_allowlist = vec!["data.example:8443".into()];
        let egress = EgressAllowList::derive(&cfg, &storage);
        assert_eq!(egress.endpoints.len(), 1);
        assert_eq!(egress.endpoints[0], EgressEndpoint::new("data.example", Some(8443)));
    }

    #[test]
    fn auto_backend_resolves_per_platform() {
        let b = effective_backend(SandboxBackend::Auto);
        if cfg!(target_os = "android") {
            assert_eq!(b, SandboxBackend::Android);
        } else if cfg!(target_os = "ios") {
            assert_eq!(b, SandboxBackend::Ios);
        } else if cfg!(target_os = "linux") {
            assert_eq!(b, SandboxBackend::CgroupsSeccomp);
        } else if cfg!(target_os = "macos") {
            assert_eq!(b, SandboxBackend::MacosSeatbelt);
        } else if cfg!(target_os = "windows") {
            assert_eq!(b, SandboxBackend::WindowsJobObject);
        } else if cfg!(unix) {
            assert_eq!(b, SandboxBackend::Rlimit);
        } else {
            assert_eq!(b, SandboxBackend::None);
        }
    }

    #[test]
    fn explicit_unsupported_backend_degrades_to_noop_off_platform() {
        // On this macOS host, requesting the Windows backend must not panic and
        // must degrade to a working sandbox object (no-op), not break the build.
        let mut cfg = sandbox_cfg();
        cfg.backend = SandboxBackend::WindowsJobObject;
        let sb = build(&cfg);
        // macOS is not windows, so windows-jobobject degrades to noop here.
        if cfg!(windows) {
            assert_eq!(sb.backend(), SandboxBackend::WindowsJobObject);
        } else {
            assert_eq!(sb.backend(), SandboxBackend::None);
        }
    }

    #[test]
    fn disabled_config_builds_noop() {
        let cfg = SandboxConfig::default(); // enabled = false
        let sb = build(&cfg);
        assert_eq!(sb.backend(), SandboxBackend::None);
        assert_eq!(sb.name(), "noop");
    }

    #[test]
    fn spec_denies_network_when_remote_access_off() {
        let storage = StorageConfig::default(); // remote off
        let cfg = sandbox_cfg();
        let spec = SandboxSpec::resolve(
            &cfg,
            &storage,
            JobBudget {
                memory_bytes: 1 << 20,
                threads: 1,
            },
            "/tmp/job",
        );
        assert!(!spec.allow_network);
    }
}
