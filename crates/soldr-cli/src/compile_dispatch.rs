//! Shared `Request::Compile` dispatch logic (issue #1081).
//!
//! Lifted out of `wrapper.rs` so it is reachable by both the existing
//! soldr-as-wrapper hot path and multicall `zccache-soldr` dispatch.
//! Both callers do the same thing:
//!
//! 1. Build a `CompileRequest` from the rustc-style argv they were
//!    invoked with.
//! 2. Forward the current process env minus known session noise
//!    (see [`is_compile_env_var`]) — build-script-emitted
//!    `cargo:rustc-env` vars have arbitrary names and MUST survive.
//! 3. Connect to the soldr-daemon IPC socket / Windows named pipe.
//! 4. On failure to connect, spawn the daemon detached and retry
//!    within a configurable budget (default 30s for production —
//!    `SOLDR_DAEMON_SPAWN_RETRY_BUDGET_MS` overrides for tests).
//! 5. Stream the `CompileStdoutChunk` / `CompileStderrChunk` frames
//!    back to the caller's stdout/stderr until `CompileDone` arrives,
//!    then return the captured exit code.
//!
//! ## Hang-safety contract (issue #1081)
//!
//! Every IPC call has an explicit per-call timeout. The spawn-retry
//! loop has a hard wall-clock budget (no "wait forever for daemon to
//! come up"). The retry budget defaults to 30 s in production and is
//! overridable via `SOLDR_DAEMON_SPAWN_RETRY_BUDGET_MS` so tests can
//! force a sub-second fast-fail and never wedge the test runner.
//!
//! The daemon-side already drops the inflight compile if the wrapper
//! disconnects (see PR #1078 / `daemon::server::race_against_disconnect`),
//! so a ctrl-c in the shim or wrapper cancels the embedded rustc
//! invocation cleanly.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::core::{SoldrError, SoldrPaths};
use crate::daemon::client;
use crate::daemon::protocol::{CompileLifecycle, CompileRequest};

/// Escape-hatch env var (soldr#1300): when truthy (`1`, `true`, ...),
/// a daemon-unavailability failure after the retry budget hard-fails
/// the build (the pre-#1300 behavior) instead of degrading to a direct
/// uncached rustc exec. Intended for CI lanes that want to *catch*
/// daemon regressions rather than silently build uncached.
pub const SOLDR_DAEMON_REQUIRED_ENV_VAR: &str = "SOLDR_DAEMON_REQUIRED";

/// Env var that overrides the spawn-retry budget in milliseconds.
/// Default production budget is `DEFAULT_SPAWN_RETRY_BUDGET_MS`; tests
/// set this to a sub-second value so the wait-for-daemon loop fails
/// fast against a known-absent socket instead of wedging the test
/// runner.
pub const SOLDR_DAEMON_SPAWN_RETRY_BUDGET_MS_ENV_VAR: &str = "SOLDR_DAEMON_SPAWN_RETRY_BUDGET_MS";

/// Default 30-second budget for the daemon-spawn + retry loop.
///
/// Embedded zccache cold-start (redb open, cache root init, depgraph
/// load) can take several seconds on a first-ever boot in a fresh
/// container — this budget needs to comfortably cover that worst case.
pub const DEFAULT_SPAWN_RETRY_BUDGET_MS: u64 = 30_000;

/// Per-attempt retry interval — keeps the loop cheap when the daemon
/// is coming up but doesn't waste an extra ~hundred-ms after the
/// socket starts accepting.
const RETRY_INTERVAL: Duration = Duration::from_millis(100);

/// Re-run the spawn-herd-protected daemon startup while a wrapper is still
/// inside its connect budget. A detached child can exit after `spawn()`
/// succeeds; probing alone can never recover from that case.
const RESPAWN_INTERVAL: Duration = Duration::from_secs(5);

/// Freshness window for the cross-process daemon-unavailable marker.
/// When one wrapper proves the daemon cannot be reached, sibling rustc
/// wrapper processes skip the full spawn retry budget for this window
/// and fall back directly after one cheap probe.
const DAEMON_UNAVAILABLE_MARKER_TTL: Duration = Duration::from_secs(300);

/// Predicate for the env-var filter (Phase 5c from #981, reworked for
/// correctness after the `cargo:rustc-env` regression that broke the
/// linux-arm-musl cross-compile lane — run 28574600982).
///
/// Returns `true` for env vars that must be forwarded in the
/// `Request::Compile` payload so the daemon-spawned rustc sees the
/// same environment cargo gave the wrapper process.
///
/// ## Why this is a noise DENYLIST, not an allowlist
///
/// Cargo forwards `cargo:rustc-env=<NAME>=<value>` lines emitted by a
/// crate's build script as environment variables **on the rustc
/// invocation only** — and `<NAME>` is arbitrary (crgx sets
/// `CRGX_TARGET`, vergen sets `VERGEN_*`, shadow-rs sets `SHADOW_*`,
/// ...). The crate then reads them back with `env!()` at compile
/// time. An allowlist can never enumerate those names, and dropping
/// one turns into a hard `error: environment variable ... not defined
/// at compile time` inside the daemon-spawned rustc. That is exactly
/// what broke the `linux-arm-musl` cross-compile lane: the crgx
/// source-build fallback died on `env!("CRGX_TARGET")` because the
/// old allowlist filtered the var out of the compile request.
///
/// So the filter now drops only *known* interactive-session noise
/// (shell prompt state, desktop-session plumbing) that no compiler,
/// build script, or proc-macro legitimately reads. Everything else —
/// including vars we have never heard of — is forwarded. The
/// standalone zccache wrapper client forwards its **entire** env
/// (see `_vender/zccache/crates/zccache/src/cli/commands/wrap/env.rs`),
/// so this keeps the embedded-daemon path consistent with the managed
/// binary path. The Phase 5c payload-size concern is preserved by the
/// denylist (session noise still never crosses the wire) and by the
/// fact that zccache's fingerprint only hashes `CARGO_*` vars, so the
/// extra forwarded vars do not churn cache keys.
pub fn is_compile_env_var(name: &str) -> bool {
    // Desktop / login-session plumbing. Prefix matches.
    const NOISE_PREFIXES: &[&str] = &[
        "XDG_",     // XDG_SESSION_TYPE, XDG_RUNTIME_DIR, ...
        "GNOME_",   // GNOME_TERMINAL_SERVICE, ...
        "GDM",      // GDM_LANG, GDMSESSION
        "DESKTOP_", // DESKTOP_SESSION-adjacent
    ];
    if NOISE_PREFIXES.iter().any(|p| name.starts_with(p)) {
        return false;
    }
    // Shell-prompt / terminal-session state. Exact matches.
    !matches!(
        name,
        "PROMPT"
            | "PS1"
            | "PS2"
            | "PS4"
            | "OLDPWD"
            | "SHLVL"
            | "DISPLAY"
            | "WAYLAND_DISPLAY"
            | "DBUS_SESSION_BUS_ADDRESS"
            | "SESSION_MANAGER"
            | "LS_COLORS"
            | "LSCOLORS"
            | "PSModulePath"
            | "ChocolateyInstall"
            | "ChocolateyLastPathUpdate"
            | "WSL_DISTRO_NAME"
            | "WSL_INTEROP"
            | "WT_SESSION"
            | "WT_PROFILE_ID"
            | "WINDOWID"
            | "COLORTERM"
            | "VTE_VERSION"
    )
}

/// Build a `CompileRequest` from a rustc-style argv. `argv[0]` is the
/// rustc path (or clippy-driver, etc.) and `argv[1..]` are the
/// arguments rustc receives. This is the SAME shape both
/// soldr-as-RUSTC_WRAPPER and multicall `zccache-soldr` dispatch are
/// invoked with — RUSTC_WRAPPER's contract is `[wrapper, rustc_path,
/// ...rustc_args]`, and after the wrapper entry has stripped
/// argv[0] we get the [rustc_path, ...rustc_args] shape this function
/// expects.
pub fn build_compile_request(rustc_argv: &[String]) -> CompileRequest {
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let env: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| is_compile_env_var(k))
        .collect();
    CompileRequest {
        args: rustc_argv.to_vec(),
        cwd,
        env,
        stdin: Vec::new(),
        lifecycle: build_compile_lifecycle(rustc_argv),
        ipc_busy_retries: 0,
    }
}

fn build_compile_lifecycle(rustc_argv: &[String]) -> Option<CompileLifecycle> {
    let session_id = std::env::var(crate::cache_lib::SOLDR_BUILD_SESSION_ID_ENV_VAR)
        .ok()?
        .parse::<u64>()
        .ok()?;
    let rustc_args = rustc_argv.get(1..).unwrap_or_default();
    let target_dir = crate::cache_lib::target_registry::resolve_workspace_target_dir(rustc_args)?;
    Some(CompileLifecycle {
        session_id,
        crate_name: parse_crate_name(rustc_args)
            .unwrap_or("unknown")
            .to_string(),
        target_dir: target_dir.display().to_string(),
        started_at_ms: current_unix_ms(),
    })
}

fn parse_crate_name(args: &[String]) -> Option<&str> {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--crate-name" {
            return args.next().map(String::as_str);
        }
        if let Some(value) = arg.strip_prefix("--crate-name=") {
            return Some(value);
        }
    }
    None
}

fn current_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// Read the spawn-retry budget from env (overridable for tests) or
/// fall back to the production default. Clamped to at least 100 ms so
/// no caller can accidentally disable retries entirely.
pub fn resolved_spawn_retry_budget() -> Duration {
    let ms = std::env::var(SOLDR_DAEMON_SPAWN_RETRY_BUDGET_MS_ENV_VAR)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_SPAWN_RETRY_BUDGET_MS)
        .max(100);
    Duration::from_millis(ms)
}

/// Terminal failure from the compile dispatch (soldr#1300).
///
/// Structured (rather than a pre-formatted `SoldrError::Other`) so the
/// caller can distinguish "the daemon never became reachable" — which
/// should degrade to a direct uncached rustc exec — from "the daemon
/// answered but something else went wrong", which must stay a hard
/// failure.
#[derive(Debug)]
pub enum DispatchError {
    /// The spawn-retry budget elapsed without a `CompileDone` reply.
    /// `last_err` is the most recent per-attempt failure (always
    /// `Some` in practice — the first pre-spawn attempt's error is
    /// captured before the retry loop starts). `spawn_err` carries the
    /// detached-spawn failure, if any, for diagnostics.
    BudgetExhausted {
        budget: Duration,
        last_err: Option<client::ClientError>,
        sock: PathBuf,
        spawn_err: Option<String>,
    },
    /// Pre-dispatch setup failed (e.g. the soldr cache root could not
    /// be resolved). Never triggers the direct-exec fallback — this is
    /// a local environment problem, not daemon unavailability.
    Setup(SoldrError),
}

impl DispatchError {
    /// `true` when the terminal condition is daemon *unavailability*
    /// (never came up / transport failure) as opposed to a healthy
    /// daemon that answered with an error.
    pub fn is_daemon_unavailable(&self) -> bool {
        match self {
            DispatchError::BudgetExhausted { last_err, .. } => match last_err {
                // Budget elapsed without a single classified attempt:
                // the daemon was certainly never reached.
                None => true,
                Some(e) => client_error_indicates_daemon_unavailable(e),
            },
            DispatchError::Setup(_) => false,
        }
    }

    /// Wall-clock budget that elapsed, in milliseconds (0 for setup
    /// failures — no retry loop ever ran).
    pub fn budget_ms(&self) -> u128 {
        match self {
            DispatchError::BudgetExhausted { budget, .. } => budget.as_millis(),
            DispatchError::Setup(_) => 0,
        }
    }

    /// Collapse into the flat `SoldrError` shape the pre-#1300 callers
    /// expect. Message format is unchanged from the original dispatch
    /// error (plus an optional `spawn_err=` suffix) so existing log
    /// grep patterns keep working.
    pub fn into_soldr_error(self) -> SoldrError {
        SoldrError::Other(self.to_string())
    }
}

impl std::fmt::Display for DispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DispatchError::BudgetExhausted {
                budget,
                last_err,
                sock,
                spawn_err,
            } => {
                write!(
                    f,
                    "soldr daemon embedded compile dispatch failed after {}ms budget: \
                     last_err={:?} sock={}",
                    budget.as_millis(),
                    last_err,
                    sock.display()
                )?;
                if let Some(spawn_err) = spawn_err {
                    write!(f, " spawn_err={spawn_err}")?;
                }
                Ok(())
            }
            DispatchError::Setup(e) => write!(f, "{e}"),
        }
    }
}

/// Classify a per-attempt [`client::ClientError`] as daemon
/// unavailability (soldr#1300).
///
/// * `NotRunning` — no endpoint at the socket/pipe path, or connect
///   refused. The canonical "daemon never came up" signal (the macOS
///   pip-wheel failure mode from #1300).
/// * `Io` — endpoint transport failure: connect timeout, read/write
///   error, daemon died mid-stream. The daemon is not usably available.
/// * `Protocol` — the daemon *answered* (an `Error` frame or an
///   unexpected frame). It is alive and responding; degrading to an
///   uncached compile would mask a real daemon-side bug, so this stays
///   a hard failure. Note an actual rustc compile FAILURE never shows
///   up here at all — it arrives as `CompileDone { exit_code != 0 }`,
///   i.e. `Ok(_)` at this layer, and is propagated as the exit code.
pub fn client_error_indicates_daemon_unavailable(e: &client::ClientError) -> bool {
    match e {
        client::ClientError::NotRunning => true,
        client::ClientError::Io(_) => true,
        client::ClientError::Protocol(_) => false,
    }
}

/// Truthy check for [`SOLDR_DAEMON_REQUIRED_ENV_VAR`]. Unset, empty,
/// `0`, `false`, `no`, `off` (case-insensitive) mean "not required" —
/// everything else opts in to the hard-fail behavior.
pub fn daemon_required() -> bool {
    std::env::var(SOLDR_DAEMON_REQUIRED_ENV_VAR)
        .ok()
        .map(|v| {
            let v = v.trim();
            !(v.is_empty()
                || v == "0"
                || v.eq_ignore_ascii_case("false")
                || v.eq_ignore_ascii_case("no")
                || v.eq_ignore_ascii_case("off"))
        })
        .unwrap_or(false)
}

/// Fallback decision (soldr#1300): degrade to a direct uncached rustc
/// exec only when the terminal condition is daemon unavailability AND
/// the caller has not opted in to hard-fail via
/// [`SOLDR_DAEMON_REQUIRED_ENV_VAR`].
pub fn should_fall_back_to_direct_rustc(failure: &DispatchError) -> bool {
    !daemon_required() && failure.is_daemon_unavailable()
}

/// Persist every fallback, but leave Cargo-front-door reporting to the parent
/// build process. Each compiler unit is a separate wrapper process, so
/// printing here for a managed build would produce hundreds of duplicate
/// lines and Cargo would persist/replay them from `.fingerprint/output-*`.
pub fn log_direct_exec_fallback_once(failure: &DispatchError) {
    static LOGGED: std::sync::Once = std::sync::Once::new();
    LOGGED.call_once(|| {
        let managed_session = std::env::var(crate::cache_lib::SOLDR_BUILD_SESSION_ID_ENV_VAR)
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .is_some();
        match SoldrPaths::new()
            .map_err(std::io::Error::other)
            .and_then(|paths| append_compile_daemon_fallback_event(&paths, failure))
        {
            Ok(path) if !managed_session => {
                eprintln!(
                    "soldr: compiler cache unavailable; using direct compiler. Full details: {}",
                    path.display()
                );
            }
            Ok(_) => {}
            Err(error) => {
                eprintln!(
                    "soldr: compiler cache unavailable; using direct compiler. \
                     Failed to write full details: {error}; reason={failure}"
                );
            }
        }
    });
}

pub(crate) fn compile_daemon_fallback_log_path(paths: &SoldrPaths) -> PathBuf {
    paths
        .root
        .join("logs")
        .join("compile-daemon-fallbacks.jsonl")
}

#[derive(Clone, Debug, Default)]
pub(crate) struct CompileFallbackCursor {
    len: u64,
    tail_anchor: Vec<u8>,
}

/// Capture a cheap append cursor for the fallback journal.
///
/// The tail anchor distinguishes a normal append from log
/// truncation/replacement without hashing the whole (potentially large) file
/// on every front-door invocation.
pub(crate) fn compile_daemon_fallback_cursor(paths: &SoldrPaths) -> CompileFallbackCursor {
    const ANCHOR_BYTES: u64 = 512;

    let path = compile_daemon_fallback_log_path(paths);
    let Ok(mut file) = std::fs::File::open(path) else {
        return CompileFallbackCursor::default();
    };
    let Ok(len) = file.metadata().map(|metadata| metadata.len()) else {
        return CompileFallbackCursor::default();
    };
    let anchor_start = len.saturating_sub(ANCHOR_BYTES);
    if file.seek(SeekFrom::Start(anchor_start)).is_err() {
        return CompileFallbackCursor::default();
    }
    let mut tail_anchor = Vec::with_capacity((len - anchor_start) as usize);
    if file.read_to_end(&mut tail_anchor).is_err() {
        return CompileFallbackCursor::default();
    }
    CompileFallbackCursor { len, tail_anchor }
}

/// Append one durable build-integrity record when a wrapper bypasses the
/// managed cache. This has its own homogeneous JSONL stream so consumers of
/// the established cargo-abort schema never have to interpret a tagged union.
fn append_compile_daemon_fallback_event(
    paths: &SoldrPaths,
    failure: &DispatchError,
) -> std::io::Result<PathBuf> {
    let path = compile_daemon_fallback_log_path(paths);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let ts_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let session_id = std::env::var(crate::cache_lib::SOLDR_BUILD_SESSION_ID_ENV_VAR)
        .ok()
        .and_then(|value| value.parse::<u64>().ok());
    let mut record = serde_json::to_vec(&serde_json::json!({
        "schema_version": 1,
        "event": "compile_daemon_fallback",
        "ts_ms": ts_ms,
        "session_id": session_id,
        "pid": std::process::id(),
        "budget_ms": failure.budget_ms(),
        "reason": failure.to_string(),
    }))
    .map_err(std::io::Error::other)?;
    record.push(b'\n');
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?
        .write_all(&record)?;
    Ok(path)
}

/// Count fallback records appended for one Cargo-front-door build session.
///
/// Other builds may append concurrently, so the byte offset only limits the
/// scan; `session_id` is the authoritative correlation key.
pub(crate) fn compile_daemon_fallback_count_since(
    paths: &SoldrPaths,
    cursor: &CompileFallbackCursor,
    session_id: u64,
) -> std::io::Result<(usize, PathBuf)> {
    let path = compile_daemon_fallback_log_path(paths);
    let mut file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((0, path)),
        Err(error) => return Err(error),
    };
    let len = file.metadata()?.len();
    let anchor_start = cursor.len.saturating_sub(cursor.tail_anchor.len() as u64);
    let anchor_matches = if len < cursor.len {
        false
    } else {
        file.seek(SeekFrom::Start(anchor_start))?;
        let mut current_anchor = vec![0; cursor.tail_anchor.len()];
        file.read_exact(&mut current_anchor)?;
        current_anchor == cursor.tail_anchor
    };
    let scan_start = if anchor_matches { cursor.len } else { 0 };
    file.seek(SeekFrom::Start(scan_start))?;
    let mut appended = String::new();
    file.read_to_string(&mut appended)?;
    let mut count = 0;
    for (index, line) in appended.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event: serde_json::Value = serde_json::from_str(line).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "malformed fallback record {} in {}: {error}",
                    index + 1,
                    path.display()
                ),
            )
        })?;
        if event["event"] == "compile_daemon_fallback"
            && event["session_id"].as_u64() == Some(session_id)
        {
            count += 1;
        }
    }
    Ok((count, path))
}

/// Direct uncached exec of the rustc-style argv (soldr#1300 fallback
/// used by the `zccache-soldr` shim; the soldr-as-wrapper path reuses
/// its own richer direct-exec code in `wrapper.rs`). Passes stdio
/// through untouched so rustc's stdout/stderr/exit code reach cargo
/// exactly as a non-wrapped invocation would.
pub fn direct_exec_rustc(rustc_argv: &[String]) -> Result<i32, SoldrError> {
    let (tool, args) = rustc_argv
        .split_first()
        .ok_or_else(|| SoldrError::Other("direct rustc fallback: empty rustc argv".to_string()))?;
    let mut command = std::process::Command::new(tool);
    command.args(args);
    crate::core::suppress_windows_console_window(&mut command);
    let status = command.status()?;
    Ok(status.code().unwrap_or(1))
}

fn daemon_unavailable_marker_path(paths: &SoldrPaths) -> PathBuf {
    crate::cache_lib::soldr_daemon_dir(paths).join("compile-daemon-unavailable")
}

fn remember_daemon_unavailable(marker_path: &Path) {
    remember_daemon_unavailable_with_reason(marker_path, None);
}

/// Persist the terminal dispatch error alongside the cooldown marker.
///
/// The marker is intentionally human-readable: it is a cross-process
/// circuit-breaker, not an authoritative state database. Keeping the original
/// error here means a sibling rustc wrapper that skips the retry budget can
/// still report why the first wrapper failed instead of manufacturing a new
/// zero-duration failure.
fn remember_daemon_unavailable_with_reason(marker_path: &Path, reason: Option<&str>) {
    if let Some(parent) = marker_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut contents = String::from("daemon unavailable\n");
    if let Some(reason) = reason {
        let reason = reason.replace(['\r', '\n'], " ");
        contents.push_str("reason=");
        contents.push_str(&reason);
        contents.push('\n');
    }
    let _ = std::fs::write(marker_path, contents);
}

fn read_daemon_unavailable_reason(marker_path: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(marker_path).ok()?;
    contents
        .lines()
        .find_map(|line| line.strip_prefix("reason="))
        .filter(|reason| !reason.is_empty())
        .map(ToOwned::to_owned)
}

fn forget_daemon_unavailable(marker_path: &Path) {
    let _ = std::fs::remove_file(marker_path);
}

fn daemon_unavailable_marker_is_fresh_at(marker_path: &Path, now: SystemTime) -> bool {
    let Ok(metadata) = std::fs::metadata(marker_path) else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    match now.duration_since(modified) {
        Ok(age) if age <= DAEMON_UNAVAILABLE_MARKER_TTL => true,
        Ok(_) => {
            forget_daemon_unavailable(marker_path);
            false
        }
        Err(_) => true,
    }
}

fn should_skip_retry_after_recent_daemon_unavailable(
    marker_path: &Path,
    first_err: &client::ClientError,
) -> bool {
    !daemon_required()
        && client_error_indicates_daemon_unavailable(first_err)
        && daemon_unavailable_marker_is_fresh_at(marker_path, SystemTime::now())
}

fn recent_daemon_unavailable_error(
    sock_path: &Path,
    marker_path: &Path,
    first_err: client::ClientError,
) -> DispatchError {
    let marker_reason = read_daemon_unavailable_reason(marker_path);
    let spawn_err = match marker_reason {
        Some(reason) => format!(
            "recent daemon-unavailable marker present; skipped spawn retry; \
             prior_failure={reason}"
        ),
        None => "recent daemon-unavailable marker present; skipped spawn retry".to_string(),
    };
    DispatchError::BudgetExhausted {
        budget: Duration::ZERO,
        last_err: Some(first_err),
        sock: sock_path.to_path_buf(),
        spawn_err: Some(spawn_err),
    }
}

fn record_dispatch_result_marker(marker_path: &Path, result: &Result<i32, DispatchError>) {
    match result {
        Ok(_) => forget_daemon_unavailable(marker_path),
        Err(err) if err.is_daemon_unavailable() => {
            remember_daemon_unavailable_with_reason(marker_path, Some(&err.to_string()))
        }
        Err(_) => {}
    }
}

/// Dispatch a rustc-style compile to soldr-daemon's embedded zccache
/// service and stream the reply back to `stdout`/`stderr`. Returns
/// the rustc exit code on success.
///
/// `rustc_argv` is the [rustc_path, ...rustc_args] tail described in
/// [`build_compile_request`].
///
/// Retry behavior: first attempt uses the current process's
/// daemon-connect timeout. On any failure, spawn the daemon detached
/// and retry every [`RETRY_INTERVAL`] until either a successful
/// `CompileDone` arrives OR the wall-clock budget (per
/// [`resolved_spawn_retry_budget`]) elapses. **The function never
/// blocks forever** — that's the hang-safety contract.
pub fn dispatch_compile<O, E>(
    rustc_argv: &[String],
    stdout: O,
    stderr: E,
) -> Result<i32, SoldrError>
where
    O: Write,
    E: Write,
{
    dispatch_compile_detailed(rustc_argv, stdout, stderr).map_err(DispatchError::into_soldr_error)
}

/// Structured-error variant of [`dispatch_compile`] (soldr#1300). The
/// caller can consult [`should_fall_back_to_direct_rustc`] on the
/// error to decide between the direct-exec degradation and a hard
/// fail.
pub fn dispatch_compile_detailed<O, E>(
    rustc_argv: &[String],
    stdout: O,
    stderr: E,
) -> Result<i32, DispatchError>
where
    O: Write,
    E: Write,
{
    let paths = SoldrPaths::new().map_err(|e| {
        DispatchError::Setup(SoldrError::Other(format!("resolve soldr paths: {e}")))
    })?;
    let sock = client::default_sock_path(&paths);
    let marker = daemon_unavailable_marker_path(&paths);
    let mut counted = SilenceDetectingWriter::new(stderr);
    let result = dispatch_compile_with_sock_and_marker_detailed(
        &sock,
        Some(&marker),
        rustc_argv,
        stdout,
        &mut counted,
        true,
    );
    if let Ok(exit_code) = result.as_ref() {
        counted.report_if_silently_failed(*exit_code);
    }
    result
}

/// Wraps the caller's stderr sink and records whether the dispatched
/// compile wrote anything to it.
///
/// A rustc failure never surfaces as a [`DispatchError`] — it arrives as
/// `CompileDone { exit_code != 0 }` and is propagated as a bare exit code
/// (see [`client_error_indicates_daemon_unavailable`]). When the daemon
/// also relays no stderr, Cargo prints `error: could not compile <crate>`
/// with an empty cause and the user has nothing to act on.
///
/// That is the shape of soldr#1857, where ~2.8% of dispatched compiles on
/// Windows fail this way while the same workload through
/// `soldr --no-cache` is clean. Detecting "failed **and** said nothing" is
/// what separates that fault from an ordinary compile error, so it is
/// worth the one counter.
struct SilenceDetectingWriter<W> {
    inner: W,
    bytes_written: u64,
}

impl<W: Write> SilenceDetectingWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            bytes_written: 0,
        }
    }

    /// Emit a diagnostic when the compile failed without explaining why.
    ///
    /// Deliberately hedged: a non-zero exit with no output is *usually*
    /// the #1857 fault, but claiming it always is would be wrong, so the
    /// message states the observation first and the likely cause second.
    fn report_if_silently_failed(&mut self, exit_code: i32) {
        if exit_code == 0 || self.bytes_written > 0 {
            return;
        }
        let _ = writeln!(
            self.inner,
            "soldr: rustc exited {exit_code} without emitting any diagnostics.\n\
             soldr: the compile was dispatched to soldr-daemon and failed before it \
             could report a reason — see soldr#1857.\n\
             soldr: retrying usually succeeds; `soldr --no-cache cargo ...` bypasses \
             the daemon entirely."
        );
        let _ = self.inner.flush();
    }
}

impl<W: Write> Write for SilenceDetectingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.bytes_written += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn dispatch_compile_with_sock_and_marker_detailed<O, E>(
    sock_path: &Path,
    marker_path: Option<&Path>,
    rustc_argv: &[String],
    mut stdout: O,
    mut stderr: E,
    spawn_on_first_failure: bool,
) -> Result<i32, DispatchError>
where
    O: Write,
    E: Write,
{
    let req = build_compile_request(rustc_argv);

    // First try: daemon may already be running.
    let first_err =
        match client::compile_streaming(sock_path, req.clone(), &mut stdout, &mut stderr) {
            Ok(done) => {
                if let Some(marker_path) = marker_path {
                    forget_daemon_unavailable(marker_path);
                }
                return Ok(done.exit_code);
            }
            Err(e) => e,
        };

    if let Some(marker_path) = marker_path {
        if should_skip_retry_after_recent_daemon_unavailable(marker_path, &first_err) {
            return Err(recent_daemon_unavailable_error(
                sock_path,
                marker_path,
                first_err,
            ));
        }
    }

    let budget = resolved_spawn_retry_budget();
    let start = Instant::now();
    let deadline = start + budget;
    let mut prepared_spawn = None;
    let spawn_err = if spawn_on_first_failure {
        match crate::binaries::ensure_daemon_executable_handoff() {
            Ok(_) => match crate::daemon::lifecycle::try_spawn_detached_until(Some(deadline)) {
                Ok(prepared) => {
                    prepared_spawn = prepared;
                    None
                }
                Err(error) => Some(format!("initial daemon spawn failed: {error:?}")),
            },
            Err(error) => Some(format!(
                "canonical soldr-daemon handoff materialization failed: {error}"
            )),
        }
    } else {
        None
    };
    let result = retry_within_budget(
        sock_path,
        req,
        first_err,
        spawn_err,
        stdout,
        stderr,
        RespawnPlan {
            prepared: prepared_spawn,
            start,
            budget,
        },
    );
    if let Some(marker_path) = marker_path {
        record_dispatch_result_marker(marker_path, &result);
    }
    result
}

#[cfg(test)]
fn dispatch_compile_with_sock_and_marker_for_test<O, E>(
    sock_path: &Path,
    marker_path: &Path,
    rustc_argv: &[String],
    stdout: O,
    stderr: E,
) -> Result<i32, DispatchError>
where
    O: Write,
    E: Write,
{
    dispatch_compile_with_sock_and_marker_detailed(
        sock_path,
        Some(marker_path),
        rustc_argv,
        stdout,
        stderr,
        false,
    )
}

/// Variant of [`dispatch_compile`] that takes an explicit socket path
/// override. Lets tests point the dispatch at a known-bad path
/// (non-existent socket) so the retry loop fails fast against the
/// configured budget — proves the no-hang contract without spinning
/// up a real daemon.
pub fn dispatch_compile_with_sock<O, E>(
    sock_path: &Path,
    rustc_argv: &[String],
    stdout: O,
    stderr: E,
) -> Result<i32, SoldrError>
where
    O: Write,
    E: Write,
{
    dispatch_compile_with_sock_detailed(sock_path, rustc_argv, stdout, stderr)
        .map_err(DispatchError::into_soldr_error)
}

/// Structured-error variant of [`dispatch_compile_with_sock`]
/// (soldr#1300). No daemon spawn is attempted — explicit-override
/// callers are tests pointing at a known socket.
pub fn dispatch_compile_with_sock_detailed<O, E>(
    sock_path: &Path,
    rustc_argv: &[String],
    mut stdout: O,
    mut stderr: E,
) -> Result<i32, DispatchError>
where
    O: Write,
    E: Write,
{
    let req = build_compile_request(rustc_argv);

    // First try without spawning — we only spawn if a stock socket path
    // is in play; for explicit-override tests, just retry within budget.
    let first_err =
        match client::compile_streaming(sock_path, req.clone(), &mut stdout, &mut stderr) {
            Ok(done) => return Ok(done.exit_code),
            Err(e) => e,
        };

    let budget = resolved_spawn_retry_budget();
    retry_within_budget(
        sock_path,
        req,
        first_err,
        None,
        stdout,
        stderr,
        RespawnPlan {
            prepared: None,
            start: Instant::now(),
            budget,
        },
    )
}

/// Shared spawn-retry loop body. Retries the streaming compile every
/// [`RETRY_INTERVAL`] until success or budget exhaustion. `first_err`
/// seeds `last_err` so a budget too small for even one loop iteration
/// still reports the pre-loop attempt's failure.
fn periodic_respawn_due(
    enabled: bool,
    error: &client::ClientError,
    elapsed: Duration,
    next_respawn_at: Duration,
) -> bool {
    enabled && client_error_indicates_daemon_unavailable(error) && elapsed >= next_respawn_at
}

fn periodic_respawn_fits_budget(elapsed: Duration, budget: Duration) -> bool {
    budget.saturating_sub(elapsed) >= RESPAWN_INTERVAL
}

fn run_periodic_respawn_if_due<F, T>(
    enabled: bool,
    error: &client::ClientError,
    elapsed: Duration,
    next_respawn_at: Duration,
    budget: Duration,
    spawn: F,
) -> Option<T>
where
    F: FnOnce() -> T,
{
    (periodic_respawn_due(enabled, error, elapsed, next_respawn_at)
        && periodic_respawn_fits_budget(elapsed, budget))
    .then(spawn)
}

struct RespawnPlan {
    prepared: Option<crate::daemon::lifecycle::PreparedDaemonSpawn>,
    start: Instant,
    budget: Duration,
}

fn retry_within_budget<O, E>(
    sock_path: &Path,
    req: CompileRequest,
    first_err: client::ClientError,
    mut spawn_err: Option<String>,
    mut stdout: O,
    mut stderr: E,
    plan: RespawnPlan,
) -> Result<i32, DispatchError>
where
    O: Write,
    E: Write,
{
    let RespawnPlan {
        prepared: prepared_spawn,
        start,
        budget,
    } = plan;
    let deadline = start + budget;
    let mut last_err: Option<client::ClientError> = Some(first_err);
    let mut next_respawn_at = RESPAWN_INTERVAL;
    while start.elapsed() < budget {
        std::thread::sleep(RETRY_INTERVAL);
        match client::compile_streaming(sock_path, req.clone(), &mut stdout, &mut stderr) {
            Ok(done) => return Ok(done.exit_code),
            Err(e) => {
                last_err = Some(e);
                if let Some(respawn_result) = run_periodic_respawn_if_due(
                    prepared_spawn.is_some(),
                    last_err.as_ref().expect("last error assigned above"),
                    start.elapsed(),
                    next_respawn_at,
                    budget,
                    || {
                        crate::daemon::lifecycle::try_spawn_detached_prepared_until(
                            prepared_spawn
                                .as_ref()
                                .expect("respawn is enabled only with a prepared image"),
                            deadline,
                        )
                    },
                ) {
                    if let Err(error) = respawn_result {
                        spawn_err = Some(format!("periodic respawn failed: {error:?}"));
                    }
                    next_respawn_at = start.elapsed() + RESPAWN_INTERVAL;
                }
            }
        }
    }
    Err(DispatchError::BudgetExhausted {
        budget,
        last_err,
        sock: sock_path.to_path_buf(),
        spawn_err,
    })
}

fn compile_dispatch_failure_message(
    budget: Duration,
    last_err: Option<&client::ClientError>,
    sock_path: &Path,
) -> String {
    let last_err = last_err
        .map(describe_compile_dispatch_error)
        .unwrap_or_else(|| "no daemon reply was received before the retry budget expired".into());
    format!(
        "soldr embedded zccache compile dispatch failed after {}ms: the soldr-daemon embedded \
         zccache cache daemon was unreachable, stopped responding, or died mid-compile. \
         last_err={last_err}; sock={}. Recovery: inspect `soldr logs paths` and `soldr daemon \
         status`; to keep the build moving while investigating, rerun with `soldr --no-cache cargo \
         ...` or `ZCCACHE_DISABLE=1`; set `SOLDR_COMPILE_REPLY_TIMEOUT_SECS=<seconds>` to tune the \
         per-compile no-response backstop for large release/LTO builds.",
        budget.as_millis(),
        sock_path.display()
    )
}

fn describe_compile_dispatch_error(err: &client::ClientError) -> String {
    match err {
        client::ClientError::NotRunning => {
            "daemon endpoint is not running or refused the connection".into()
        }
        client::ClientError::Protocol(message) => {
            format!("daemon protocol error: {message}")
        }
        client::ClientError::Io(err) => {
            use std::io::ErrorKind;

            match err.kind() {
                ErrorKind::UnexpectedEof | ErrorKind::BrokenPipe | ErrorKind::ConnectionReset => {
                    format!(
                        "daemon connection closed while the compile was in flight ({err}); the \
                         embedded zccache cache daemon may have crashed or been killed"
                    )
                }
                ErrorKind::TimedOut | ErrorKind::WouldBlock => {
                    format!(
                        "daemon did not return a compile reply before the timeout ({err}); the \
                         embedded zccache cache daemon may be wedged"
                    )
                }
                _ => format!("daemon IPC I/O error: {err}"),
            }
        }
    }
}

/// Wrapper around [`client::compile_streaming`] that mirrors the
/// signature wrapper.rs used to call directly. Re-export with this
/// name keeps the bin-side `compile_via_daemon` callers backward-
/// compatible after the lift.
pub fn compile_via_daemon(rustc_argv: &[String]) -> Result<i32, SoldrError> {
    dispatch_compile(rustc_argv, std::io::stdout(), std::io::stderr())
}

/// Structured-error variant of [`compile_via_daemon`] (soldr#1300).
/// Used by the wrapper hot path so it can degrade to its own direct-
/// exec code when the daemon is unavailable.
pub fn compile_via_daemon_detailed(rustc_argv: &[String]) -> Result<i32, DispatchError> {
    dispatch_compile_detailed(rustc_argv, std::io::stdout(), std::io::stderr())
}

// Re-export the daemon-side type so callers don't have to reach into
// `daemon::client` directly. Useful for the shim's optional logging.
pub use client::CompileDoneInfo as DispatchInfo;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timed_test;

    /// soldr#1857 — a dispatched compile that fails *and* says nothing is
    /// the fault signature worth calling out. These three cases pin the
    /// boundary: only failure-with-silence gets the extra diagnostic.
    timed_test!(silent_failure_gets_an_explanatory_diagnostic, {
        let mut sink: Vec<u8> = Vec::new();
        let mut writer = SilenceDetectingWriter::new(&mut sink);
        writer.report_if_silently_failed(1);

        let text = String::from_utf8(sink).expect("utf8");
        assert!(
            text.contains("without emitting any diagnostics"),
            "expected the silence to be named; got:
{text}"
        );
        assert!(
            text.contains("soldr#1857"),
            "expected a pointer to the tracking issue; got:
{text}"
        );
        assert!(
            text.contains("--no-cache"),
            "expected the documented bypass; got:
{text}"
        );
    });

    timed_test!(failure_that_produced_output_is_left_alone, {
        let mut sink: Vec<u8> = Vec::new();
        let mut writer = SilenceDetectingWriter::new(&mut sink);
        // Whatever rustc already said about the failure.
        writer
            .write_all(b"error[E0308]: mismatched types
")
            .expect("write");
        writer.report_if_silently_failed(1);

        let text = String::from_utf8(sink).expect("utf8");
        assert!(
            !text.contains("soldr#1857"),
            "an ordinary compile error must not be blamed on #1857; got:
{text}"
        );
        assert_eq!(text, "error[E0308]: mismatched types
");
    });

    timed_test!(successful_silent_compile_is_left_alone, {
        let mut sink: Vec<u8> = Vec::new();
        let mut writer = SilenceDetectingWriter::new(&mut sink);
        // The overwhelmingly common case: a cache hit says nothing and succeeds.
        writer.report_if_silently_failed(0);

        assert!(sink.is_empty(), "exit 0 must stay silent");
    });

    timed_test!(byte_count_tracks_partial_writes, {
        let mut sink: Vec<u8> = Vec::new();
        let mut writer = SilenceDetectingWriter::new(&mut sink);
        writer.write_all(b"abc").expect("write");
        writer.write_all(b"de").expect("write");

        assert_eq!(writer.bytes_written, 5);
    });
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// Serializes the env-mutating tests in this module so they don't
    /// race the shared `SOLDR_DAEMON_SPAWN_RETRY_BUDGET_MS` /
    /// `SOLDR_DAEMON_REQUIRED` env vars. Lower-overhead than pulling in
    /// `serial_test` as a dep; the affected tests all complete in
    /// single-digit ms once they hold the lock.
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    /// Lock-guard helper that also saves + restores one env var so
    /// tests leave the global env in the state they found it. Holds
    /// the shared [`ENV_MUTEX`], so at most one guard can exist at a
    /// time — acquire additional vars through [`EnvVarGuard::also`].
    struct EnvVarGuard<'a> {
        _guard: std::sync::MutexGuard<'a, ()>,
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl<'a> EnvVarGuard<'a> {
        fn acquire(name: &'static str) -> Self {
            let _guard = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
            let prior = std::env::var_os(name);
            std::env::remove_var(name);
            Self {
                _guard,
                saved: vec![(name, prior)],
            }
        }

        /// Track (save + clear) an additional env var under the same
        /// mutex hold.
        fn also(&mut self, name: &'static str) {
            let prior = std::env::var_os(name);
            std::env::remove_var(name);
            self.saved.push((name, prior));
        }

        fn set(&self, name: &str, value: &str) {
            debug_assert!(
                self.saved.iter().any(|(n, _)| *n == name),
                "EnvVarGuard::set on untracked var {name}"
            );
            std::env::set_var(name, value);
        }
    }

    impl<'a> Drop for EnvVarGuard<'a> {
        fn drop(&mut self) {
            for (name, prior) in self.saved.drain(..) {
                match prior {
                    Some(v) => std::env::set_var(name, v),
                    None => std::env::remove_var(name),
                }
            }
        }
    }

    /// Back-compat shim for the pre-#1300 budget-var guard call sites.
    struct BudgetEnvGuard<'a>(EnvVarGuard<'a>);

    impl<'a> BudgetEnvGuard<'a> {
        fn acquire() -> Self {
            Self(EnvVarGuard::acquire(
                SOLDR_DAEMON_SPAWN_RETRY_BUDGET_MS_ENV_VAR,
            ))
        }

        fn set(&self, value: &str) {
            self.0
                .set(SOLDR_DAEMON_SPAWN_RETRY_BUDGET_MS_ENV_VAR, value);
        }
    }

    timed_test!(
        is_compile_env_var_recognizes_msvc_link_vars,
        Duration::from_secs(5),
        {
            // Issue #1079 — the MSVC discovery sets LIB / INCLUDE / LIBPATH
            // on the wrapper process. The dispatch's env filter must
            // forward them to the daemon so the embedded rustc invocation
            // gets the same env. Regression-test that explicitly.
            for v in ["LIB", "INCLUDE", "LIBPATH", "PATH"] {
                assert!(is_compile_env_var(v), "{v} must be forwarded to daemon");
            }
        }
    );

    timed_test!(
        is_compile_env_var_forwards_build_script_rustc_env_vars,
        Duration::from_secs(5),
        {
            // Regression test for the linux-arm-musl cross-compile lane
            // failure (run 28574600982): crgx's build.rs emits
            // `cargo:rustc-env=CRGX_TARGET=<triple>` and reads it back
            // with `env!("CRGX_TARGET")`. Cargo sets such vars only on
            // the rustc process env; if the dispatch filter drops them,
            // the daemon-spawned rustc fails with `error: environment
            // variable ... not defined at compile time`. The names are
            // arbitrary, so the filter must be a noise denylist that
            // forwards anything it does not recognize.
            for v in [
                "CRGX_TARGET",
                "VERGEN_GIT_SHA",
                "SHADOW_RS",
                "SOME_CRATE_CUSTOM_COMPILE_TIME_VAR",
            ] {
                assert!(
                    is_compile_env_var(v),
                    "{v} must be forwarded to the daemon — build-script \
                     `cargo:rustc-env` vars have arbitrary names and dropping \
                     them breaks `env!()` in daemon-spawned rustc"
                );
            }
        }
    );

    timed_test!(
        is_compile_env_var_forwards_apple_sdk_vars,
        Duration::from_secs(5),
        {
            // Regression test for the darwin cross-compile lanes (run
            // 28574600982). `soldr build` and explicit legacy
            // `soldr cargo zigbuild --target *-apple-darwin` export SDKROOT;
            // rustc reads it to locate the Apple SDK when linking (it
            // appends `-isysroot <sdk>` to the cc-style linker), and the
            // zig-cc linker shim reads it again for the SDK library
            // search path. The daemon replays the filtered env into
            // rustc (env_clear + replay), so dropping any of these
            // makes every `-lobjc` / `-framework` link fail with
            // "unable to find dynamic system library 'objc'".
            for v in ["SDKROOT", "DEVELOPER_DIR", "MACOSX_DEPLOYMENT_TARGET"] {
                assert!(is_compile_env_var(v), "{v} must be forwarded to daemon");
            }
        }
    );

    timed_test!(
        is_compile_env_var_drops_common_noise,
        Duration::from_secs(5),
        {
            for v in [
                "PROMPT",
                "PSModulePath",
                "ChocolateyInstall",
                "WSL_DISTRO_NAME",
            ] {
                assert!(!is_compile_env_var(v), "{v} should be dropped");
            }
        }
    );

    timed_test!(
        build_compile_request_filters_env_and_carries_args,
        Duration::from_secs(5),
        {
            std::env::set_var("CARGO_PKG_NAME_TEST_DISPATCH", "soldr-cli-test");
            let argv = vec!["rustc".to_string(), "--version".to_string()];
            let req = build_compile_request(&argv);
            std::env::remove_var("CARGO_PKG_NAME_TEST_DISPATCH");

            assert_eq!(req.args, argv);
            assert!(
                req.env
                    .iter()
                    .any(|(k, _)| k == "CARGO_PKG_NAME_TEST_DISPATCH"),
                "CARGO_*-prefixed env var must survive the filter"
            );
        }
    );

    timed_test!(
        resolved_spawn_retry_budget_respects_override,
        Duration::from_secs(5),
        {
            let g = BudgetEnvGuard::acquire();
            g.set("250");
            let budget = resolved_spawn_retry_budget();
            drop(g);
            assert_eq!(budget, Duration::from_millis(250));
        }
    );

    timed_test!(
        resolved_spawn_retry_budget_clamps_to_min_100ms,
        Duration::from_secs(5),
        {
            // Defense in depth: no caller can disable retries entirely
            // by passing 0 — the loop must still get at least one shot.
            let g = BudgetEnvGuard::acquire();
            g.set("0");
            let budget = resolved_spawn_retry_budget();
            drop(g);
            assert!(
                budget >= Duration::from_millis(100),
                "budget {budget:?} must be clamped to at least 100ms"
            );
        }
    );

    timed_test!(
        compile_dispatch_failure_message_names_daemon_death_and_recovery,
        Duration::from_secs(5),
        {
            let err = client::ClientError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "daemon closed pipe",
            ));
            let message = compile_dispatch_failure_message(
                Duration::from_secs(30),
                Some(&err),
                Path::new("/tmp/soldr-daemon.sock"),
            );

            assert!(
                message.contains("embedded zccache cache daemon"),
                "diagnostic must name the cache daemon failure: {message}"
            );
            assert!(
                message.contains("died mid-compile") || message.contains("crashed or been killed"),
                "diagnostic must distinguish daemon death from a bare rustc failure: {message}"
            );
            for needle in [
                "soldr logs paths",
                "soldr daemon status",
                "soldr --no-cache cargo",
                "ZCCACHE_DISABLE=1",
                "SOLDR_COMPILE_REPLY_TIMEOUT_SECS",
            ] {
                assert!(
                    message.contains(needle),
                    "diagnostic missing recovery hint {needle:?}: {message}"
                );
            }
        }
    );

    // The TDD acceptance test for the no-hang contract: point dispatch
    // at a socket path that cannot possibly accept, set the budget to
    // 250 ms, expect the call to return an Err in well under 2 seconds.
    // The `timed_test!` 10-second deadline is the belt-and-suspenders —
    // if a regression makes the dispatch ignore the budget, the test
    // hangs there and the watchdog fires.
    timed_test!(
        dispatch_compile_with_sock_fails_within_budget_on_dead_socket,
        Duration::from_secs(15),
        {
            let g = BudgetEnvGuard::acquire();
            // 250 ms budget; we expect dispatch to fail-fast.
            g.set("250");

            // A path that cannot resolve to a live socket / pipe on any
            // platform. On Windows the leading `\\.\pipe\` prefix is
            // mandatory for named pipes, so a Unix-style path under TMP
            // cannot accept; on Unix the path simply does not exist.
            let dead = if cfg!(windows) {
                PathBuf::from(r"\\.\pipe\soldr-test-no-such-pipe-12345")
            } else {
                std::env::temp_dir().join("soldr-test-no-such-sock-12345")
            };
            // Make sure no leftover artifact from a prior test is on disk.
            let _ = std::fs::remove_file(&dead);

            let argv = vec!["rustc".to_string(), "--version".to_string()];
            let mut stdout: Vec<u8> = Vec::new();
            let mut stderr: Vec<u8> = Vec::new();

            let start = Instant::now();
            let result =
                dispatch_compile_with_sock_detailed(&dead, &argv, &mut stdout, &mut stderr);
            let elapsed = start.elapsed();
            drop(g);

            let err = result.expect_err("dispatch should error on dead socket");
            // soldr#1300 — a dead socket is the canonical daemon-
            // unavailability terminal condition; the detailed error
            // must classify as fallback-eligible.
            assert!(
                err.is_daemon_unavailable(),
                "dead-socket failure must classify as daemon-unavailable: {err:?}"
            );
            // Windows runtime spawn overhead per attempt can push elapsed
            // well past the configured budget. The contract is "the loop
            // honors the budget" — i.e. we don't sit at the 30 s default —
            // not a tight stopwatch on absolute time. A 5 s ceiling is
            // generous enough to absorb tokio runtime startup while still
            // catching a regression that ignores the budget entirely.
            assert!(
                elapsed < Duration::from_secs(5),
                "dispatch took {elapsed:?} on a dead socket with a 250 ms budget — \
             this is the no-hang contract: the retry loop MUST honor the \
             SOLDR_DAEMON_SPAWN_RETRY_BUDGET_MS budget instead of waiting \
             out the 30s default"
            );
        }
    );

    timed_test!(
        daemon_unavailable_marker_freshness_expires_and_cleans_stale,
        Duration::from_secs(5),
        {
            let temp = tempfile::tempdir().expect("tempdir");
            let marker = temp.path().join("compile-daemon-unavailable");

            assert!(!daemon_unavailable_marker_is_fresh_at(
                &marker,
                SystemTime::now()
            ));
            remember_daemon_unavailable(&marker);
            assert!(
                daemon_unavailable_marker_is_fresh_at(&marker, SystemTime::now()),
                "new marker should be fresh"
            );

            let stale_now =
                SystemTime::now() + DAEMON_UNAVAILABLE_MARKER_TTL + Duration::from_secs(1);
            assert!(
                !daemon_unavailable_marker_is_fresh_at(&marker, stale_now),
                "marker should expire after the cooldown ttl"
            );
            assert!(
                !marker.exists(),
                "stale marker should be cleaned up best-effort"
            );
        }
    );

    timed_test!(
        periodic_respawn_is_due_only_for_daemon_unavailability,
        Duration::from_secs(5),
        {
            let due = RESPAWN_INTERVAL;
            assert!(!periodic_respawn_due(
                true,
                &client::ClientError::NotRunning,
                due - Duration::from_millis(1),
                due,
            ));
            assert!(periodic_respawn_due(
                true,
                &client::ClientError::NotRunning,
                due,
                due,
            ));
            assert!(!periodic_respawn_due(
                false,
                &client::ClientError::NotRunning,
                due,
                due,
            ));
            assert!(!periodic_respawn_due(
                true,
                &client::ClientError::Protocol("compile rejected".into()),
                due,
                due,
            ));
            assert!(periodic_respawn_fits_budget(
                Duration::from_secs(5),
                Duration::from_secs(30),
            ));
            assert!(!periodic_respawn_fits_budget(
                Duration::from_secs(26),
                Duration::from_secs(30),
            ));
            let calls = std::cell::Cell::new(0);
            let result = run_periodic_respawn_if_due(
                true,
                &client::ClientError::NotRunning,
                Duration::from_secs(5),
                Duration::from_secs(5),
                Duration::from_secs(30),
                || {
                    calls.set(calls.get() + 1);
                    "spawned"
                },
            );
            assert_eq!(result, Some("spawned"));
            assert_eq!(calls.get(), 1);

            let late = run_periodic_respawn_if_due(
                true,
                &client::ClientError::NotRunning,
                Duration::from_secs(26),
                Duration::from_secs(5),
                Duration::from_secs(30),
                || calls.set(calls.get() + 1),
            );
            assert!(late.is_none());
            assert_eq!(calls.get(), 1, "late respawn must not exceed the budget");
        }
    );

    timed_test!(
        compile_daemon_fallback_appends_structured_build_integrity_event,
        Duration::from_secs(5),
        {
            let session_guard =
                EnvVarGuard::acquire(crate::cache_lib::SOLDR_BUILD_SESSION_ID_ENV_VAR);
            let temp = tempfile::tempdir().expect("tempdir");
            let paths = SoldrPaths::with_root(temp.path().to_path_buf());
            let failure = DispatchError::BudgetExhausted {
                budget: Duration::from_secs(30),
                last_err: Some(client::ClientError::NotRunning),
                sock: temp.path().join("sock"),
                spawn_err: Some("detached child exited".into()),
            };

            let path = append_compile_daemon_fallback_event(&paths, &failure)
                .expect("append fallback event");
            let line = std::fs::read_to_string(&path).expect("read fallback event");
            let event: serde_json::Value =
                serde_json::from_str(line.trim()).expect("valid fallback JSONL");

            assert_eq!(event["schema_version"], 1);
            assert_eq!(event["event"], "compile_daemon_fallback");
            assert_eq!(event["budget_ms"], 30_000);
            assert_eq!(event["pid"], std::process::id());
            assert!(event["ts_ms"].as_u64().is_some());
            assert!(event["session_id"].is_null());
            assert!(path.ends_with("compile-daemon-fallbacks.jsonl"));
            assert!(event["reason"]
                .as_str()
                .expect("reason string")
                .contains("detached child exited"));

            session_guard.set(crate::cache_lib::SOLDR_BUILD_SESSION_ID_ENV_VAR, "42");
            append_compile_daemon_fallback_event(&paths, &failure)
                .expect("append session-correlated fallback event");
            let lines = std::fs::read_to_string(&path).expect("read fallback events");
            let correlated: serde_json::Value =
                serde_json::from_str(lines.lines().nth(1).expect("second fallback event"))
                    .expect("valid correlated fallback JSONL");
            assert_eq!(correlated["session_id"], 42);
            drop(session_guard);
        }
    );

    timed_test!(
        compile_daemon_fallback_count_filters_concurrent_sessions,
        Duration::from_secs(5),
        {
            let session_guard =
                EnvVarGuard::acquire(crate::cache_lib::SOLDR_BUILD_SESSION_ID_ENV_VAR);
            let temp = tempfile::tempdir().expect("tempdir");
            let paths = SoldrPaths::with_root(temp.path().to_path_buf());
            let failure = DispatchError::BudgetExhausted {
                budget: Duration::from_millis(250),
                last_err: Some(client::ClientError::NotRunning),
                sock: temp.path().join("sock"),
                spawn_err: None,
            };

            session_guard.set(crate::cache_lib::SOLDR_BUILD_SESSION_ID_ENV_VAR, "7");
            append_compile_daemon_fallback_event(&paths, &failure).expect("seed old event");
            let cursor = compile_daemon_fallback_cursor(&paths);

            session_guard.set(crate::cache_lib::SOLDR_BUILD_SESSION_ID_ENV_VAR, "42");
            append_compile_daemon_fallback_event(&paths, &failure).expect("append session event");
            session_guard.set(crate::cache_lib::SOLDR_BUILD_SESSION_ID_ENV_VAR, "99");
            append_compile_daemon_fallback_event(&paths, &failure)
                .expect("append concurrent event");
            session_guard.set(crate::cache_lib::SOLDR_BUILD_SESSION_ID_ENV_VAR, "42");
            append_compile_daemon_fallback_event(&paths, &failure)
                .expect("append second session event");

            let (count, path) = compile_daemon_fallback_count_since(&paths, &cursor, 42)
                .expect("count session events");
            assert_eq!(count, 2);
            assert_eq!(path, compile_daemon_fallback_log_path(&paths));
            drop(session_guard);
        }
    );

    timed_test!(
        compile_daemon_fallback_count_recovers_from_log_replacement,
        Duration::from_secs(5),
        {
            let session_guard =
                EnvVarGuard::acquire(crate::cache_lib::SOLDR_BUILD_SESSION_ID_ENV_VAR);
            let temp = tempfile::tempdir().expect("tempdir");
            let paths = SoldrPaths::with_root(temp.path().to_path_buf());
            let failure = DispatchError::BudgetExhausted {
                budget: Duration::from_millis(250),
                last_err: Some(client::ClientError::NotRunning),
                sock: temp.path().join("sock"),
                spawn_err: None,
            };

            session_guard.set(crate::cache_lib::SOLDR_BUILD_SESSION_ID_ENV_VAR, "7");
            append_compile_daemon_fallback_event(&paths, &failure).expect("seed old event");
            let cursor = compile_daemon_fallback_cursor(&paths);
            std::fs::remove_file(compile_daemon_fallback_log_path(&paths))
                .expect("rotate old fallback log");

            session_guard.set(crate::cache_lib::SOLDR_BUILD_SESSION_ID_ENV_VAR, "42");
            append_compile_daemon_fallback_event(&paths, &failure)
                .expect("append event to replacement log");

            let (count, _) = compile_daemon_fallback_count_since(&paths, &cursor, 42)
                .expect("scan replacement from byte zero");
            assert_eq!(count, 1);
            drop(session_guard);
        }
    );

    timed_test!(
        compile_daemon_fallback_count_reports_malformed_appended_record,
        Duration::from_secs(5),
        {
            let temp = tempfile::tempdir().expect("tempdir");
            let paths = SoldrPaths::with_root(temp.path().to_path_buf());
            let cursor = compile_daemon_fallback_cursor(&paths);
            let path = compile_daemon_fallback_log_path(&paths);
            std::fs::create_dir_all(path.parent().unwrap()).expect("create log directory");
            std::fs::write(&path, b"{not-json}\n").expect("write malformed record");

            let error = compile_daemon_fallback_count_since(&paths, &cursor, 42)
                .expect_err("malformed appended record must be visible");
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
            assert!(error.to_string().contains("malformed fallback record"));
        }
    );

    timed_test!(
        daemon_unavailable_failure_records_marker_for_sibling_wrappers,
        Duration::from_secs(10),
        {
            let mut g = EnvVarGuard::acquire(SOLDR_DAEMON_SPAWN_RETRY_BUDGET_MS_ENV_VAR);
            g.also(SOLDR_DAEMON_REQUIRED_ENV_VAR);
            g.set(SOLDR_DAEMON_SPAWN_RETRY_BUDGET_MS_ENV_VAR, "100");

            let temp = tempfile::tempdir().expect("tempdir");
            let marker = temp.path().join("compile-daemon-unavailable");
            let dead = if cfg!(windows) {
                PathBuf::from(r"\\.\pipe\soldr-test-no-such-pipe-marker-record")
            } else {
                temp.path().join("no-such-sock")
            };
            let argv = vec!["rustc".to_string(), "--version".to_string()];
            let mut stdout: Vec<u8> = Vec::new();
            let mut stderr: Vec<u8> = Vec::new();

            let result = dispatch_compile_with_sock_and_marker_for_test(
                &dead,
                &marker,
                &argv,
                &mut stdout,
                &mut stderr,
            );
            drop(g);

            let err = result.expect_err("dead socket should fail");
            assert!(
                err.is_daemon_unavailable(),
                "dead socket should be classified as daemon-unavailable: {err:?}"
            );
            assert!(
                marker.exists(),
                "daemon-unavailable failure should leave a marker for sibling wrappers"
            );
            let marker_contents = std::fs::read_to_string(&marker).expect("read marker");
            assert!(
                marker_contents.contains("reason=soldr daemon embedded compile dispatch failed"),
                "marker should preserve the terminal dispatch error for sibling wrappers: {marker_contents:?}"
            );
        }
    );

    timed_test!(
        recent_daemon_unavailable_marker_skips_spawn_retry_budget,
        Duration::from_secs(10),
        {
            let mut g = EnvVarGuard::acquire(SOLDR_DAEMON_SPAWN_RETRY_BUDGET_MS_ENV_VAR);
            g.also(SOLDR_DAEMON_REQUIRED_ENV_VAR);
            g.set(SOLDR_DAEMON_SPAWN_RETRY_BUDGET_MS_ENV_VAR, "5000");

            let temp = tempfile::tempdir().expect("tempdir");
            let marker = temp.path().join("compile-daemon-unavailable");
            remember_daemon_unavailable(&marker);
            let dead = if cfg!(windows) {
                PathBuf::from(r"\\.\pipe\soldr-test-no-such-pipe-marker-skip")
            } else {
                temp.path().join("no-such-sock")
            };
            let argv = vec!["rustc".to_string(), "--version".to_string()];
            let mut stdout: Vec<u8> = Vec::new();
            let mut stderr: Vec<u8> = Vec::new();

            let start = Instant::now();
            let result = dispatch_compile_with_sock_and_marker_for_test(
                &dead,
                &marker,
                &argv,
                &mut stdout,
                &mut stderr,
            );
            let elapsed = start.elapsed();
            drop(g);

            let err = result.expect_err("dead socket should fail");
            assert_eq!(
                err.budget_ms(),
                0,
                "fresh marker should skip the retry budget entirely: {err:?}"
            );
            assert!(
                elapsed < Duration::from_secs(1),
                "fresh marker should avoid a per-rustc 5s retry storm; elapsed={elapsed:?}"
            );
        }
    );

    timed_test!(
        recent_marker_error_includes_original_failure,
        Duration::from_secs(5),
        {
            let g = EnvVarGuard::acquire(SOLDR_DAEMON_REQUIRED_ENV_VAR);
            let temp = tempfile::tempdir().expect("tempdir");
            let marker = temp.path().join("compile-daemon-unavailable");
            remember_daemon_unavailable_with_reason(
                &marker,
                Some("daemon startup failed: state.redb: Database already open"),
            );
            let dead = if cfg!(windows) {
                PathBuf::from(r"\\.\pipe\soldr-test-no-such-pipe-marker-reason")
            } else {
                temp.path().join("no-such-sock")
            };
            let argv = vec!["rustc".to_string(), "--version".to_string()];
            let mut stdout: Vec<u8> = Vec::new();
            let mut stderr: Vec<u8> = Vec::new();

            let result = dispatch_compile_with_sock_and_marker_for_test(
                &dead,
                &marker,
                &argv,
                &mut stdout,
                &mut stderr,
            );
            drop(g);

            let err = result.expect_err("dead socket should fail");
            let rendered = err.to_string();
            assert!(
                rendered.contains(
                    "prior_failure=daemon startup failed: state.redb: Database already open"
                ),
                "sibling fallback should surface the original marker failure: {rendered}"
            );
        }
    );

    // ---------------------------------------------------------------
    // soldr#1300 — direct-exec fallback classification + env gate
    // ---------------------------------------------------------------

    fn budget_exhausted_with(last_err: Option<client::ClientError>) -> DispatchError {
        DispatchError::BudgetExhausted {
            budget: Duration::from_millis(250),
            last_err,
            sock: PathBuf::from("/tmp/soldr-test-sock"),
            spawn_err: None,
        }
    }

    timed_test!(
        client_error_classification_for_direct_exec_fallback,
        Duration::from_secs(5),
        {
            // Transport/availability errors trigger the fallback...
            assert!(client_error_indicates_daemon_unavailable(
                &client::ClientError::NotRunning
            ));
            assert!(client_error_indicates_daemon_unavailable(
                &client::ClientError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "connect timed out"
                ))
            ));
            // ...but a daemon that ANSWERED (even with an error frame)
            // is alive — degrading would mask a real daemon bug.
            assert!(!client_error_indicates_daemon_unavailable(
                &client::ClientError::Protocol("daemon-side error".into())
            ));
        }
    );

    timed_test!(
        dispatch_error_unavailability_classification,
        Duration::from_secs(5),
        {
            assert!(budget_exhausted_with(Some(client::ClientError::NotRunning))
                .is_daemon_unavailable());
            assert!(
                budget_exhausted_with(Some(client::ClientError::Io(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "daemon died mid-stream"
                ))))
                .is_daemon_unavailable()
            );
            // No attempt error at all — the daemon was never reached.
            assert!(budget_exhausted_with(None).is_daemon_unavailable());
            // A responding daemon must stay a hard failure.
            assert!(!budget_exhausted_with(Some(client::ClientError::Protocol(
                "unexpected frame".into()
            )))
            .is_daemon_unavailable());
            // Setup failures are local problems, never fallback bait.
            assert!(!DispatchError::Setup(crate::core::SoldrError::Other(
                "resolve soldr paths: boom".into()
            ))
            .is_daemon_unavailable());
        }
    );

    timed_test!(daemon_required_env_gate, Duration::from_secs(5), {
        let g = EnvVarGuard::acquire(SOLDR_DAEMON_REQUIRED_ENV_VAR);
        // Unset → fallback allowed.
        assert!(!daemon_required());
        // Truthy values restore the hard-fail behavior.
        for v in ["1", "true", "TRUE", "yes", "on"] {
            g.set(SOLDR_DAEMON_REQUIRED_ENV_VAR, v);
            assert!(daemon_required(), "SOLDR_DAEMON_REQUIRED={v} must gate");
        }
        // Falsy spellings keep the fallback enabled.
        for v in ["0", "false", "no", "off", "", "  "] {
            g.set(SOLDR_DAEMON_REQUIRED_ENV_VAR, v);
            assert!(
                !daemon_required(),
                "SOLDR_DAEMON_REQUIRED={v:?} must NOT gate"
            );
        }
    });

    timed_test!(
        should_fall_back_combines_classification_and_env_gate,
        Duration::from_secs(5),
        {
            let g = EnvVarGuard::acquire(SOLDR_DAEMON_REQUIRED_ENV_VAR);

            // Default: unavailability → fall back.
            assert!(should_fall_back_to_direct_rustc(&budget_exhausted_with(
                Some(client::ClientError::NotRunning)
            )));
            // Default: responding daemon → hard fail.
            assert!(!should_fall_back_to_direct_rustc(&budget_exhausted_with(
                Some(client::ClientError::Protocol("daemon-side error".into()))
            )));

            // Escape hatch: SOLDR_DAEMON_REQUIRED=1 restores hard-fail
            // even for pure unavailability.
            g.set(SOLDR_DAEMON_REQUIRED_ENV_VAR, "1");
            assert!(!should_fall_back_to_direct_rustc(&budget_exhausted_with(
                Some(client::ClientError::NotRunning)
            )));
        }
    );

    timed_test!(
        dispatch_error_message_keeps_pre_1300_format,
        Duration::from_secs(5),
        {
            // Existing CI log grep patterns key on this exact prefix —
            // lock it in.
            let msg = budget_exhausted_with(Some(client::ClientError::NotRunning)).to_string();
            assert!(
                msg.starts_with(
                    "soldr daemon embedded compile dispatch failed after 250ms budget:"
                ),
                "unexpected message shape: {msg}"
            );
            assert!(msg.contains("last_err=Some(NotRunning)"), "{msg}");
        }
    );
}
