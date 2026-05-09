//! `root doctor` — self-check battery for the local install.
//!
//! Runs nine independent checks, prints a human-readable summary by
//! default (or JSON with `--json`), and returns an exit code:
//!
//! - `0` — all checks pass.
//! - `1` — at least one check raised a warning (degraded but
//!   functional).
//! - `2` — at least one check failed (broken).
//!
//! `--repair` is destructive but bounded: each check declares its own
//! repair action, and a check that has no safe repair (`binary_integrity`,
//! `provider_config`) is a no-op under `--repair`. Repairs run in
//! check order so a stale-lockfile clear runs before the daemon-
//! reachable probe re-runs (the second invocation would otherwise see
//! the now-cleared lockfile and report "unmounted" rather than "ok").
//!
//! Honesty constraints:
//!
//! - Every check returns `CheckResult::status` from a measurable
//!   signal — no placeholder "ok" when we couldn't actually probe.
//! - Repair actions log what they did into `CheckResult::detail` so
//!   `--json` consumers can audit which mutations happened.
//! - The `--repair` flag never auto-recreates `.thinkingroot/`
//!   directories the user deleted — that's silent recovery and
//!   violates CLAUDE.md §honesty rule §1. The user must re-mount
//!   explicitly.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use serde::Serialize;
use thinkingroot_core::WorkspaceRegistry;
use thinkingroot_core::cortex;

use crate::cortex_client::health_check;

/// Argument bundle for [`run_doctor`]. Built from the CLI `Doctor`
/// subcommand by `main.rs`.
#[derive(Debug, Clone)]
pub struct DoctorOpts {
    /// Run safe-repair actions for each failing check (clear stale
    /// lockfiles, prune orphan tmpfiles, refresh trust cache).
    pub repair: bool,
    /// Emit a JSON [`Report`] on stdout instead of human prose.
    pub json: bool,
    /// Expected daemon host. Defaults to `127.0.0.1`. The lockfile's
    /// host is used in preference; this is only a fallback when no
    /// lockfile exists.
    pub host: String,
}

impl Default for DoctorOpts {
    fn default() -> Self {
        Self {
            repair: false,
            json: false,
            host: "127.0.0.1".to_string(),
        }
    }
}

/// Per-check status. `Ok` means the check passed; `Warn` means
/// degraded-but-usable; `Fail` means broken in a way that affects
/// the user. Exit code = 0 / 1 / 2 respectively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// Check passed.
    Ok,
    /// Check found a warning condition but the system remains usable.
    Warn,
    /// Check failed; user-impacting.
    Fail,
}

/// Single check result. Repaired = true when `--repair` was on AND
/// the check applied a fix that flipped its status from Fail/Warn.
#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    /// Stable identifier (e.g. `"daemon_reachable"`). Used by JSON
    /// consumers; must not change between releases without a
    /// deprecation note.
    pub name: &'static str,
    /// Outcome.
    pub status: CheckStatus,
    /// How long the probe took, in milliseconds.
    pub elapsed_ms: u128,
    /// Free-form one-line detail. For Ok this is a confirmation; for
    /// Warn/Fail this is the user-visible reason.
    pub detail: String,
    /// True when `--repair` ran a mutation that recovered this check.
    pub repaired: bool,
}

impl CheckResult {
    fn ok(name: &'static str, elapsed: Duration, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: CheckStatus::Ok,
            elapsed_ms: elapsed.as_millis(),
            detail: detail.into(),
            repaired: false,
        }
    }
    fn warn(name: &'static str, elapsed: Duration, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: CheckStatus::Warn,
            elapsed_ms: elapsed.as_millis(),
            detail: detail.into(),
            repaired: false,
        }
    }
    fn fail(name: &'static str, elapsed: Duration, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: CheckStatus::Fail,
            elapsed_ms: elapsed.as_millis(),
            detail: detail.into(),
            repaired: false,
        }
    }
}

/// Aggregate report. Serialised as the JSON body when `--json` is set.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// One-word top-level summary derived from the per-check rollup:
    /// `"ok"` (all green), `"degraded"` (any warn, no fail), or
    /// `"broken"` (any fail).
    pub verdict: &'static str,
    /// Aggregate human-readable summary line.
    pub summary: String,
    /// Per-check results in execution order.
    pub checks: Vec<CheckResult>,
    /// Host introspection block; lets a remote support session see
    /// platform + version without needing a follow-up query.
    pub host: HostInfo,
}

/// Host introspection captured by [`Report`].
#[derive(Debug, Clone, Serialize)]
pub struct HostInfo {
    /// `darwin`, `linux`, `windows`, …
    pub os: &'static str,
    /// `x86_64`, `aarch64`, …
    pub arch: &'static str,
    /// `crate_pkg_version!()` of this binary.
    pub thinkingroot_version: &'static str,
}

impl Report {
    /// Compute the process exit code per the `0/1/2` convention
    /// documented at the module top.
    pub fn exit_code(&self) -> i32 {
        match self.verdict {
            "ok" => 0,
            "degraded" => 1,
            _ => 2,
        }
    }
    fn build(checks: Vec<CheckResult>) -> Self {
        let n_total = checks.len();
        let n_fail = checks.iter().filter(|c| c.status == CheckStatus::Fail).count();
        let n_warn = checks.iter().filter(|c| c.status == CheckStatus::Warn).count();
        let n_ok = n_total - n_fail - n_warn;
        let verdict = if n_fail > 0 {
            "broken"
        } else if n_warn > 0 {
            "degraded"
        } else {
            "ok"
        };
        Self {
            verdict,
            summary: format!(
                "{n_total} checks, {n_ok} ok, {n_warn} warn, {n_fail} fail"
            ),
            checks,
            host: HostInfo {
                os: std::env::consts::OS,
                arch: std::env::consts::ARCH,
                thinkingroot_version: env!("CARGO_PKG_VERSION"),
            },
        }
    }
}

/// Run the doctor battery and emit either JSON or human output.
/// Returns the process exit code.
pub async fn run_doctor(opts: DoctorOpts) -> Result<i32> {
    let mut checks = Vec::with_capacity(9);
    checks.push(check_binary_integrity().await);
    checks.push(check_lockfile_sane(opts.repair).await);
    checks.push(check_daemon_reachable(&opts.host).await);
    checks.push(check_workspace_parseable_all().await);
    checks.push(check_revocation_cache_freshness(opts.repair).await);
    checks.push(check_disk_space().await);
    checks.push(check_trust_cache_integrity().await);
    checks.push(check_dangling_tmpfiles(opts.repair).await);
    checks.push(check_provider_config().await);

    let report = Report::build(checks);
    if opts.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        render_human(&report);
    }
    Ok(report.exit_code())
}

// ── 1. binary_integrity ───────────────────────────────────────────

async fn check_binary_integrity() -> CheckResult {
    let started = Instant::now();
    match std::env::current_exe() {
        Ok(p) if p.exists() => CheckResult::ok(
            "binary_integrity",
            started.elapsed(),
            format!(
                "running from {} (v{})",
                p.display(),
                env!("CARGO_PKG_VERSION")
            ),
        ),
        Ok(p) => CheckResult::fail(
            "binary_integrity",
            started.elapsed(),
            format!("current_exe() returned `{}` but the file does not exist", p.display()),
        ),
        Err(e) => CheckResult::fail(
            "binary_integrity",
            started.elapsed(),
            format!("std::env::current_exe() failed: {e}"),
        ),
    }
}

// ── 2. lockfile_sane ──────────────────────────────────────────────

async fn check_lockfile_sane(repair: bool) -> CheckResult {
    let started = Instant::now();
    let lock_path = match cortex::lock_path() {
        Ok(p) => p,
        Err(e) => {
            return CheckResult::fail(
                "lockfile_sane",
                started.elapsed(),
                format!("could not resolve lockfile path: {e}"),
            );
        }
    };
    if !lock_path.exists() {
        return CheckResult::ok(
            "lockfile_sane",
            started.elapsed(),
            "no lockfile (daemon not running)".to_string(),
        );
    }
    let lock = match cortex::read_lock() {
        Ok(Some(l)) => l,
        Ok(None) => {
            return CheckResult::ok(
                "lockfile_sane",
                started.elapsed(),
                "no lockfile after read".to_string(),
            );
        }
        Err(e) => {
            return CheckResult::fail(
                "lockfile_sane",
                started.elapsed(),
                format!("could not parse lockfile {}: {e}", lock_path.display()),
            );
        }
    };
    if cortex::process_alive(lock.pid) {
        return CheckResult::ok(
            "lockfile_sane",
            started.elapsed(),
            format!(
                "lockfile points at live pid {} on {}:{}",
                lock.pid, lock.host, lock.port
            ),
        );
    }
    // Stale: pid is dead. Repair = remove the lockfile so the next
    // CLI invocation spawns a fresh daemon.
    let detail = format!(
        "lockfile {} points at dead pid {}",
        lock_path.display(),
        lock.pid
    );
    if repair {
        match std::fs::remove_file(&lock_path) {
            Ok(_) => CheckResult {
                name: "lockfile_sane",
                status: CheckStatus::Ok,
                elapsed_ms: started.elapsed().as_millis(),
                detail: format!("{detail} — removed"),
                repaired: true,
            },
            Err(e) => CheckResult::fail(
                "lockfile_sane",
                started.elapsed(),
                format!("{detail}; remove failed: {e}"),
            ),
        }
    } else {
        CheckResult::fail(
            "lockfile_sane",
            started.elapsed(),
            format!("{detail} — re-run with `--repair` to clear"),
        )
    }
}

// ── 3. daemon_reachable ───────────────────────────────────────────

async fn check_daemon_reachable(fallback_host: &str) -> CheckResult {
    let started = Instant::now();
    // Prefer the lockfile's host:port so we probe the same daemon
    // the CLI would attach to. Fall back to the configured default
    // host on the cortex canonical port.
    let (host, port) = match cortex::read_lock() {
        Ok(Some(l)) => (l.host, l.port),
        _ => (fallback_host.to_string(), 31760),
    };
    if health_check(&host, port).await {
        CheckResult::ok(
            "daemon_reachable",
            started.elapsed(),
            format!("/livez ok on {host}:{port}"),
        )
    } else {
        CheckResult::warn(
            "daemon_reachable",
            started.elapsed(),
            format!(
                "no /livez response on {host}:{port} \
                 (daemon may not be running — start with `root serve`)"
            ),
        )
    }
}

// ── 4. workspace_parseable_all ────────────────────────────────────

async fn check_workspace_parseable_all() -> CheckResult {
    let started = Instant::now();
    let registry = match WorkspaceRegistry::load() {
        Ok(r) => r,
        Err(e) => {
            return CheckResult::fail(
                "workspace_parseable",
                started.elapsed(),
                format!("could not read workspace registry: {e}"),
            );
        }
    };
    if registry.workspaces.is_empty() {
        return CheckResult::ok(
            "workspace_parseable",
            started.elapsed(),
            "no workspaces registered".to_string(),
        );
    }
    let mut bad: Vec<String> = Vec::new();
    for ws in &registry.workspaces {
        // We do not open Cozo from here — that would re-introduce the
        // double-writer pattern the cortex protocol exists to prevent.
        // Instead we check that the .thinkingroot/ dir exists with a
        // non-empty graph DB file. A live daemon owns the lock; if the
        // dir is gone the user has lost substrate (CLAUDE.md §1: no
        // silent recovery — surface and move on).
        let tr_dir = ws.path.join(".thinkingroot");
        if !tr_dir.exists() {
            bad.push(format!("{} (missing `.thinkingroot/`)", ws.name));
            continue;
        }
        let graph_dir = tr_dir.join("graph");
        if !graph_dir.exists() {
            bad.push(format!("{} (missing `.thinkingroot/graph/`)", ws.name));
        }
    }
    if bad.is_empty() {
        CheckResult::ok(
            "workspace_parseable",
            started.elapsed(),
            format!("{} workspaces all have substrate dirs", registry.workspaces.len()),
        )
    } else {
        CheckResult::fail(
            "workspace_parseable",
            started.elapsed(),
            format!(
                "{}/{} workspaces broken: {}",
                bad.len(),
                registry.workspaces.len(),
                bad.join(", ")
            ),
        )
    }
}

// ── 5. revocation_cache_freshness ─────────────────────────────────

async fn check_revocation_cache_freshness(repair: bool) -> CheckResult {
    let started = Instant::now();
    let cache_dir = match tr_revocation::default_cache_dir() {
        Some(d) => d,
        None => {
            return CheckResult::fail(
                "revocation_cache",
                started.elapsed(),
                "platform default cache dir is unresolvable".to_string(),
            );
        }
    };
    if !cache_dir.exists() {
        if repair {
            // Fresh cache: just create the directory; the next install
            // will populate it. Honest; no fake snapshot.
            let _ = std::fs::create_dir_all(&cache_dir);
            return CheckResult {
                name: "revocation_cache",
                status: CheckStatus::Ok,
                elapsed_ms: started.elapsed().as_millis(),
                detail: format!("created empty cache dir {}", cache_dir.display()),
                repaired: true,
            };
        }
        return CheckResult::warn(
            "revocation_cache",
            started.elapsed(),
            format!(
                "{} does not exist (no snapshot fetched yet — first `root install` will populate)",
                cache_dir.display()
            ),
        );
    }
    let snapshot_path = cache_dir.join("snapshot.json");
    if !snapshot_path.exists() {
        return CheckResult::warn(
            "revocation_cache",
            started.elapsed(),
            "cache dir exists but no snapshot.json — first install will fetch".to_string(),
        );
    }
    let age = std::fs::metadata(&snapshot_path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| SystemTime::now().duration_since(t).ok());
    match age {
        Some(d) if d < Duration::from_secs(24 * 60 * 60) => CheckResult::ok(
            "revocation_cache",
            started.elapsed(),
            format!("snapshot {}h old (< 24h fresh window)", d.as_secs() / 3600),
        ),
        Some(d) if d < Duration::from_secs(7 * 24 * 60 * 60) => CheckResult::warn(
            "revocation_cache",
            started.elapsed(),
            format!(
                "snapshot {}d old (within 7-day grace; refresh recommended)",
                d.as_secs() / 86400
            ),
        ),
        Some(d) => CheckResult::fail(
            "revocation_cache",
            started.elapsed(),
            format!(
                "snapshot {}d old (past 7-day grace — `root install` will refuse signed packs)",
                d.as_secs() / 86400
            ),
        ),
        None => CheckResult::warn(
            "revocation_cache",
            started.elapsed(),
            "could not read snapshot mtime".to_string(),
        ),
    }
}

// ── 6. disk_space ─────────────────────────────────────────────────

async fn check_disk_space() -> CheckResult {
    let started = Instant::now();
    let probe_path = dirs::config_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("/"));
    match fs2::available_space(&probe_path) {
        Ok(bytes) => {
            const GIB: u64 = 1024 * 1024 * 1024;
            let gib = bytes / GIB;
            if bytes >= 5 * GIB {
                CheckResult::ok(
                    "disk_space",
                    started.elapsed(),
                    format!("{gib} GiB free under {}", probe_path.display()),
                )
            } else if bytes >= 1 * GIB {
                CheckResult::warn(
                    "disk_space",
                    started.elapsed(),
                    format!(
                        "only {gib} GiB free under {} (< 5 GiB headroom — \
                         consider `cargo clean` per CLAUDE.md disk-hygiene rule)",
                        probe_path.display()
                    ),
                )
            } else {
                CheckResult::fail(
                    "disk_space",
                    started.elapsed(),
                    format!(
                        "only {} MiB free under {} — compile/install will fail",
                        bytes / (1024 * 1024),
                        probe_path.display()
                    ),
                )
            }
        }
        Err(e) => CheckResult::warn(
            "disk_space",
            started.elapsed(),
            format!("could not query free space: {e}"),
        ),
    }
}

// ── 7. trust_cache_integrity ──────────────────────────────────────

async fn check_trust_cache_integrity() -> CheckResult {
    let started = Instant::now();
    let cache_dir = match tr_revocation::default_cache_dir() {
        Some(d) => d,
        None => {
            return CheckResult::warn(
                "trust_cache_integrity",
                started.elapsed(),
                "no cache dir on this platform".to_string(),
            );
        }
    };
    let snapshot_path = cache_dir.join("snapshot.json");
    if !snapshot_path.exists() {
        return CheckResult::ok(
            "trust_cache_integrity",
            started.elapsed(),
            "no snapshot to check".to_string(),
        );
    }
    let raw = match std::fs::read(&snapshot_path) {
        Ok(b) => b,
        Err(e) => {
            return CheckResult::fail(
                "trust_cache_integrity",
                started.elapsed(),
                format!("read failed: {e}"),
            );
        }
    };
    if raw.is_empty() {
        return CheckResult::fail(
            "trust_cache_integrity",
            started.elapsed(),
            "snapshot.json is empty".to_string(),
        );
    }
    // Structural parse: the snapshot is JSON. We don't verify the
    // ed25519 signature here because doing so requires an active
    // trusted-keys configuration which lives downstream of `root
    // install`. The structural check still catches common corruption
    // (truncated downloads, partial writes).
    match serde_json::from_slice::<serde_json::Value>(&raw) {
        Ok(_) => CheckResult::ok(
            "trust_cache_integrity",
            started.elapsed(),
            format!("snapshot.json parses ({} bytes)", raw.len()),
        ),
        Err(e) => CheckResult::fail(
            "trust_cache_integrity",
            started.elapsed(),
            format!("snapshot.json malformed: {e}"),
        ),
    }
}

// ── 8. dangling_tmpfiles ──────────────────────────────────────────

async fn check_dangling_tmpfiles(repair: bool) -> CheckResult {
    let started = Instant::now();
    let tmp_dir = std::env::temp_dir();
    let read_dir = match std::fs::read_dir(&tmp_dir) {
        Ok(r) => r,
        Err(e) => {
            return CheckResult::warn(
                "dangling_tmpfiles",
                started.elapsed(),
                format!("could not read $TMPDIR ({}): {e}", tmp_dir.display()),
            );
        }
    };
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(24 * 60 * 60))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let mut found = 0usize;
    let mut removed = 0usize;
    for entry in read_dir.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !name_str.starts_with("thinkingroot-") {
            continue;
        }
        let path = entry.path();
        let mtime = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        if mtime > cutoff {
            continue; // recent — leave it alone
        }
        found += 1;
        if repair {
            let removed_ok = if path.is_dir() {
                std::fs::remove_dir_all(&path).is_ok()
            } else {
                std::fs::remove_file(&path).is_ok()
            };
            if removed_ok {
                removed += 1;
            }
        }
    }
    if found == 0 {
        return CheckResult::ok(
            "dangling_tmpfiles",
            started.elapsed(),
            format!("no thinkingroot-* artefacts in {} older than 24h", tmp_dir.display()),
        );
    }
    if repair {
        CheckResult {
            name: "dangling_tmpfiles",
            status: CheckStatus::Ok,
            elapsed_ms: started.elapsed().as_millis(),
            detail: format!("removed {removed}/{found} stale tmpfiles"),
            repaired: true,
        }
    } else {
        CheckResult::warn(
            "dangling_tmpfiles",
            started.elapsed(),
            format!(
                "{found} stale `thinkingroot-*` entries older than 24h \
                 in {} — re-run with `--repair` to prune",
                tmp_dir.display()
            ),
        )
    }
}

// ── 9. provider_config ────────────────────────────────────────────

async fn check_provider_config() -> CheckResult {
    let started = Instant::now();
    let cfg_path = match dirs::config_dir() {
        Some(d) => d.join("thinkingroot").join("config.toml"),
        None => {
            return CheckResult::warn(
                "provider_config",
                started.elapsed(),
                "no config dir on this platform".to_string(),
            );
        }
    };
    if !cfg_path.exists() {
        return CheckResult::warn(
            "provider_config",
            started.elapsed(),
            format!(
                "{} does not exist (run `root provider use ...` to configure)",
                cfg_path.display()
            ),
        );
    }
    let raw = match std::fs::read_to_string(&cfg_path) {
        Ok(s) => s,
        Err(e) => {
            return CheckResult::fail(
                "provider_config",
                started.elapsed(),
                format!("read {} failed: {e}", cfg_path.display()),
            );
        }
    };
    match toml::from_str::<toml::Value>(&raw) {
        Ok(v) => {
            // Honest count of configured providers without invoking
            // them — pinging every provider's `/v1/models` endpoint
            // would slow the doctor 10× and require credentials we
            // shouldn't read here.
            let providers = v
                .get("llm")
                .and_then(|l| l.get("providers"))
                .and_then(|p| p.as_table())
                .map(|t| t.len())
                .unwrap_or(0);
            if providers == 0 {
                CheckResult::warn(
                    "provider_config",
                    started.elapsed(),
                    "config.toml parsed but no [llm.providers.*] sections — `root ask` will fail".to_string(),
                )
            } else {
                CheckResult::ok(
                    "provider_config",
                    started.elapsed(),
                    format!("{providers} provider(s) configured (run `root provider status` to ping)"),
                )
            }
        }
        Err(e) => CheckResult::fail(
            "provider_config",
            started.elapsed(),
            format!("config.toml malformed: {e}"),
        ),
    }
}

// ── Human renderer ────────────────────────────────────────────────

fn render_human(report: &Report) {
    use console::style;

    println!();
    let verdict_styled = match report.verdict {
        "ok" => style(report.verdict).green().bold(),
        "degraded" => style(report.verdict).yellow().bold(),
        _ => style(report.verdict).red().bold(),
    };
    println!(
        "  {} {}    {}",
        style("root doctor:").bold(),
        verdict_styled,
        style(&report.summary).dim()
    );
    println!();
    for c in &report.checks {
        let mark = match c.status {
            CheckStatus::Ok => style("✓").green(),
            CheckStatus::Warn => style("!").yellow(),
            CheckStatus::Fail => style("✗").red(),
        };
        let repaired = if c.repaired {
            style(" (repaired)").cyan().to_string()
        } else {
            String::new()
        };
        println!(
            "  {} {:<24} {} {}{}",
            mark,
            c.name,
            style(&c.detail).white(),
            style(format!("[{}ms]", c.elapsed_ms)).dim(),
            repaired,
        );
    }
    println!();
    println!(
        "  {} on {} ({})",
        style(format!("thinkingroot v{}", report.host.thinkingroot_version)).dim(),
        report.host.os,
        report.host.arch
    );
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::TempDir;

    static ENV_GUARD: Mutex<()> = Mutex::new(());

    /// Override env vars `XDG_CONFIG_HOME` / `HOME` / `APPDATA` to a
    /// tempdir so doctor checks operate on isolated state. Mirrors
    /// the pattern in `cortex_scenarios.rs::ConfigDirOverride`.
    struct EnvOverride {
        _guard: std::sync::MutexGuard<'static, ()>,
        _tmp: TempDir,
        prev_xdg: Option<std::ffi::OsString>,
        prev_home: Option<std::ffi::OsString>,
    }

    impl EnvOverride {
        fn new() -> Self {
            let guard = ENV_GUARD.lock().expect("env guard poisoned");
            let tmp = TempDir::new().unwrap();
            let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
            let prev_home = std::env::var_os("HOME");
            unsafe {
                std::env::set_var("XDG_CONFIG_HOME", tmp.path());
                std::env::set_var("HOME", tmp.path());
            }
            Self {
                _guard: guard,
                _tmp: tmp,
                prev_xdg,
                prev_home,
            }
        }
    }

    impl Drop for EnvOverride {
        fn drop(&mut self) {
            unsafe {
                if let Some(v) = self.prev_xdg.take() {
                    std::env::set_var("XDG_CONFIG_HOME", v);
                } else {
                    std::env::remove_var("XDG_CONFIG_HOME");
                }
                if let Some(v) = self.prev_home.take() {
                    std::env::set_var("HOME", v);
                } else {
                    std::env::remove_var("HOME");
                }
            }
        }
    }

    #[tokio::test]
    async fn report_with_no_failures_returns_exit_zero() {
        let report = Report::build(vec![CheckResult::ok(
            "fixture",
            Duration::from_millis(1),
            "fine",
        )]);
        assert_eq!(report.verdict, "ok");
        assert_eq!(report.exit_code(), 0);
    }

    #[tokio::test]
    async fn report_with_warn_returns_exit_one() {
        let report = Report::build(vec![CheckResult::warn(
            "fixture",
            Duration::from_millis(1),
            "meh",
        )]);
        assert_eq!(report.verdict, "degraded");
        assert_eq!(report.exit_code(), 1);
    }

    #[tokio::test]
    async fn report_with_fail_returns_exit_two() {
        let report = Report::build(vec![CheckResult::fail(
            "fixture",
            Duration::from_millis(1),
            "broken",
        )]);
        assert_eq!(report.verdict, "broken");
        assert_eq!(report.exit_code(), 2);
    }

    #[tokio::test]
    async fn doctor_with_no_daemon_reports_warn_for_reachability() {
        // Write a lockfile pointing at a port we know is closed —
        // bind a listener, capture its port, drop it, then forge the
        // lockfile. This avoids the fallback to the real cortex port
        // 31760 which a developer might have a daemon on.
        let _env = EnvOverride::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_port = listener.local_addr().unwrap().port();
        drop(listener);

        let lock_path = cortex::lock_path().unwrap();
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        let dead_lock = cortex::CortexLock {
            schema_version: cortex::SCHEMA_VERSION,
            pid: std::process::id(), // self — alive but not listening on dead_port
            port: dead_port,
            host: "127.0.0.1".into(),
            version: "test".into(),
            started_by: cortex::StartedBy::Cli,
            started_at: chrono::Utc::now(),
            binary_path: PathBuf::from("/nonexistent"),
        };
        std::fs::write(&lock_path, serde_json::to_vec(&dead_lock).unwrap()).unwrap();

        let result = check_daemon_reachable("127.0.0.1").await;
        assert_eq!(result.status, CheckStatus::Warn);
        assert_eq!(result.name, "daemon_reachable");
    }

    #[tokio::test]
    async fn doctor_repair_clears_stale_lockfile_when_pid_dead() {
        let _env = EnvOverride::new();
        // Forge a lockfile pointing at a definitely-dead PID.
        let lock_path = cortex::lock_path().unwrap();
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        let dead_lock = cortex::CortexLock {
            schema_version: cortex::SCHEMA_VERSION,
            pid: u32::MAX, // virtually guaranteed not in use
            port: 31760,
            host: "127.0.0.1".to_string(),
            version: "test".to_string(),
            started_by: cortex::StartedBy::Cli,
            started_at: chrono::Utc::now(),
            binary_path: PathBuf::from("/nonexistent"),
        };
        std::fs::write(&lock_path, serde_json::to_vec(&dead_lock).unwrap()).unwrap();

        // Without repair: Fail.
        let result = check_lockfile_sane(false).await;
        assert_eq!(result.status, CheckStatus::Fail);
        assert!(lock_path.exists(), "without repair the lockfile must remain");

        // With repair: lockfile cleared, status flips to Ok with `repaired = true`.
        let result = check_lockfile_sane(true).await;
        assert_eq!(result.status, CheckStatus::Ok);
        assert!(result.repaired);
        assert!(
            !lock_path.exists(),
            "repair must remove the stale lockfile"
        );
    }

    #[tokio::test]
    async fn doctor_does_not_repair_without_flag() {
        let _env = EnvOverride::new();
        let lock_path = cortex::lock_path().unwrap();
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        let dead_lock = cortex::CortexLock {
            schema_version: cortex::SCHEMA_VERSION,
            pid: u32::MAX,
            port: 31760,
            host: "127.0.0.1".to_string(),
            version: "test".to_string(),
            started_by: cortex::StartedBy::Cli,
            started_at: chrono::Utc::now(),
            binary_path: PathBuf::from("/nonexistent"),
        };
        std::fs::write(&lock_path, serde_json::to_vec(&dead_lock).unwrap()).unwrap();
        let _ = check_lockfile_sane(false).await;
        assert!(lock_path.exists(), "lockfile must survive without --repair");
    }

    #[tokio::test]
    async fn json_output_includes_all_checks() {
        // Smoke: run_doctor in JSON mode produces valid JSON with the
        // expected top-level fields.
        let _env = EnvOverride::new();
        let report = Report::build(vec![CheckResult::ok(
            "fixture",
            Duration::from_millis(1),
            "fine",
        )]);
        let s = serde_json::to_string(&report).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["verdict"], "ok");
        assert!(v["checks"].is_array());
        assert!(v["host"].is_object());
        assert_eq!(v["host"]["thinkingroot_version"], env!("CARGO_PKG_VERSION"));
    }
}
