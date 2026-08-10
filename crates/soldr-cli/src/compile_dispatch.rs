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

use crate::compile_diagnostics::SilenceDetectingWriter;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::core::{SoldrError, SoldrPaths};
use crate::daemon::client;
use crate::daemon::protocol::CompileRequest;

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
pub(crate) const DAEMON_UNAVAILABLE_MARKER_TTL: Duration = Duration::from_secs(300);

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
///
/// Moved to the shared daemon-side parser in soldr#2388 Step 6 (so the wrapper
/// and the SESSION codec-bridge filter env identically); re-exported here so
/// existing callers and `tests/phase5_contract.rs` keep this path.
pub use crate::daemon::compile_request::is_compile_env_var;

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
    // The argv/env→request parsing is the shared daemon-side function (soldr#2388
    // Step 6) so the wrapper and the SESSION codec-bridge parse identically. The
    // wrapper's own process cwd/env is what feeds it here; the daemon feeds a
    // SessionStart's carried cwd/env.
    crate::daemon::compile_request::build_compile_request_from(rustc_argv, cwd, std::env::vars())
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
    /// expect, EXCEPT for a daemon-unavailable failure, which gets an
    /// actionable infra-attributed message (soldr#2360) — see
    /// [`crate::daemon_infra_remedy`].
    pub fn into_soldr_error(self) -> SoldrError {
        crate::daemon_infra_remedy::into_soldr_error(self)
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
/// # The policy (soldr#1838 Phase 2, bullet 5)
///
/// `true` degrades this compile to direct rustc; `false` fails the build.
/// One question decides it:
///
/// > **Does this error mean the daemon is behaving correctly and simply
/// > cannot serve the request — or that something inside it is wrong?**
///
/// Degrade on the first. A daemon that is absent, unreachable, retiring,
/// version-skewed, or stalled cannot complete this unit no matter how many
/// retries remain, and falling back to an uncached compile loses nothing but
/// cache hits.
///
/// Hard-fail on the second. Degrading there would turn a real daemon bug into
/// a silently uncached build — the failure would stop being visible while
/// continuing to cost every user their cache. Slow, correct builds are a
/// worse outcome than a loud failure, because nobody investigates them.
///
/// Note an actual rustc compile FAILURE never reaches this function at all —
/// it arrives as `CompileDone { exit_code != 0 }`, i.e. `Ok(_)` at this
/// layer, and is propagated as the exit code.
///
/// # Applying it
///
/// | variant | degrade? | why |
/// |---|---|---|
/// | `NotRunning` | yes | no endpoint, or connect refused — the canonical "daemon never came up" signal (the macOS pip-wheel failure from #1300) |
/// | `Io` | yes | transport failure: connect timeout, read/write error, daemon died mid-stream |
/// | `VersionMismatch` | yes | deployment skew (#1853). A daemon speaking a protocol we cannot parse is, to the wrapper, indistinguishable from no daemon |
/// | `Retiring` | yes | graceful drain (#1838/#1837). It answered, it is not buggy, and it will never serve this compile |
/// | `CompileStalled` | yes | the deadline expired either way; `saw_output` changes the *advice*, never the decision |
/// | `Protocol` | **no** | the daemon answered with an `Error` or unexpected frame. It is alive and responding, so this is the case the hard-fail rule exists for |
///
/// The single `false` is the load-bearing one. `Retiring` exists precisely
/// because an orderly shutdown used to land in it and fail builds (#1837);
/// when adding a variant, ask whether it is really a daemon *defect* before
/// reaching for `false`.
pub fn client_error_indicates_daemon_unavailable(e: &client::ClientError) -> bool {
    match e {
        client::ClientError::NotRunning => true,
        client::ClientError::Io(_) => true,
        client::ClientError::Protocol(_) => false,
        // #1853: a daemon speaking a protocol we cannot parse is, from the
        // wrapper's perspective, indistinguishable from no daemon at all — it
        // can never serve a compile, however many retries remain. That is
        // deployment skew, exactly what the recovery ladder exists for, not a
        // daemon-side bug being masked (the rationale above for `Protocol`).
        client::ClientError::VersionMismatch(_) => true,
        // soldr#1838 Phase 2: the daemon said it is retiring. Same reasoning
        // as `VersionMismatch` — it answered, it is not buggy, and it will
        // never serve this compile, so degrading masks nothing. This is the
        // case that used to arrive as `Protocol` and hard-fail the build
        // during a normal graceful drain (#1837).
        client::ClientError::Retiring => true,
        // soldr#1838 bullet 4: a stalled compile degrades either way -- the
        // wrapper cannot finish it, and the retry ladder exists for exactly
        // this. `saw_output` changes the *advice*, not the decision.
        client::ClientError::CompileStalled { .. } => true,
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
    let _ = failure;
    false
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
    let exit_code = status.code().unwrap_or(1);
    // soldr#1974 -- same rationale as the `direct_exec_tool` twin: stdio is
    // inherited, so nothing else on this path can explain a DLL-init death.
    crate::host_pressure::report_process_init_failure_to_stderr(tool, exit_code);
    Ok(exit_code)
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
        // soldr#2317: ignore a marker left by a *prior* session (see marker_session).
        && !crate::marker_session::marker_predates_current_session(marker_path)
}

fn recent_daemon_unavailable_error(
    sock_path: &Path,
    marker_path: &Path,
    first_err: client::ClientError,
) -> DispatchError {
    // soldr#2317: message names the marker path + remaining cooldown so the
    // skip is observable and clearable, not a dead end.
    let spawn_err = crate::marker_session::skipped_retry_message(
        marker_path,
        read_daemon_unavailable_reason(marker_path),
    );
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
    _stdout: O,
    _stderr: E,
) -> Result<i32, DispatchError>
where
    O: Write,
    E: Write,
{
    // soldr#2388: ALL compile traffic goes through the broker
    // (client → broker SESSION relay → daemon). There is NO direct
    // client→daemon dial: a broker that cannot serve the compile is a loud,
    // infra-attributed **fail-fast**, never a silent degrade to a direct dial
    // and never a silent uncached rustc. This deliberately concentrates the
    // timeout/failure surface at the one broker (the ruled topology) instead of
    // scattering it across per-client daemon dials. `session_hot_path` writes
    // the compiler's output straight to this process's stdio and owns all
    // policy (broker-launched-daemon wait, the pre-output-vs-mid-output
    // boundary), so this only routes its terminal outcome.
    match crate::session_transport::session_hot_path(rustc_argv) {
        crate::session_transport::SessionHotPathOutcome::Served(exit_code) => Ok(exit_code),
        crate::session_transport::SessionHotPathOutcome::HardFail(err) => {
            Err(DispatchError::Setup(SoldrError::Other(format!(
                "SESSION compile failed after output began (no safe retry): {err}"
            ))))
        }
        crate::session_transport::SessionHotPathOutcome::Fallthrough => Err(DispatchError::Setup(
            SoldrError::Other(crate::daemon_infra_remedy::broker_unavailable_remedy()),
        )),
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
    let (prepared_spawn, spawn_err) = if spawn_on_first_failure {
        crate::broker_discovery_gate::spawn_or_confirm_broker_daemon(deadline)
    } else {
        (None, None)
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
        // soldr#1838 bullet 4. Slow and wedged look identical at the deadline
        // and want opposite fixes, so guessing wrong actively wastes the
        // user's time: telling someone with a 40-minute LTO link to bypass the
        // cache makes their build slower, and telling someone with a wedged
        // daemon to raise the timeout just prolongs the hang.
        client::ClientError::CompileStalled {
            saw_output,
            elapsed,
        } => {
            let secs = elapsed.as_secs();
            if *saw_output {
                format!(
                    concat!(
                        "the compile was still streaming output when the {}s reply ",
                        "deadline expired, so this looks like a long compile rather ",
                        "than a wedged daemon; raise it with ",
                        "SOLDR_COMPILE_REPLY_TIMEOUT_SECS=<seconds>"
                    ),
                    secs
                )
            } else {
                format!(
                    concat!(
                        "the daemon accepted the compile but produced no output in {}s, ",
                        "which is the wedged-cache signature (#1364) rather than a slow ",
                        "build; bypass it with `soldr --no-cache cargo ...` or ",
                        "ZCCACHE_DISABLE=1"
                    ),
                    secs
                )
            }
        }
        client::ClientError::Retiring => concat!(
            "the daemon is shutting down and declined the compile; soldr is ",
            "falling back to a direct rustc invocation for this unit"
        )
        .to_string(),
        // #1853: name the skew explicitly. Before the daemon learned to send a
        // reject record this surfaced as an opaque ECONNRESET, which read as a
        // crashed daemon and sent people looking in the wrong place.
        client::ClientError::VersionMismatch(message) => {
            format!(
                "{message}; the running daemon was built from a different soldr \
                 version — stop it (`soldr daemon stop`) or align the soldr on \
                 PATH with the one that started it"
            )
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

    // soldr#1838 bullet 5: the policy table on
    // `client_error_indicates_daemon_unavailable` is documentation, and
    // documentation rots. This is its machine-checked counterpart — every
    // variant, stated once, so a table that drifts from the code fails here.
    //
    // The match itself is already exhaustive, so a *new* variant cannot be
    // forgotten; what this catches is an existing one being reclassified
    // without the reasoning above being revisited.
    timed_test!(the_degrade_policy_matches_its_documented_table, {
        let cases: &[(&str, client::ClientError, bool)] = &[
            ("NotRunning", client::ClientError::NotRunning, true),
            (
                "Io",
                client::ClientError::Io(std::io::Error::other("transport")),
                true,
            ),
            (
                "VersionMismatch",
                client::ClientError::VersionMismatch("skew".into()),
                true,
            ),
            ("Retiring", client::ClientError::Retiring, true),
            (
                "CompileStalled",
                client::ClientError::CompileStalled {
                    saw_output: false,
                    elapsed: std::time::Duration::from_secs(1),
                },
                true,
            ),
            // The load-bearing `false`: a daemon that answered wrongly is a
            // daemon defect, and degrading would hide it behind a silently
            // uncached build.
            (
                "Protocol",
                client::ClientError::Protocol("bad frame".into()),
                false,
            ),
        ];
        for (name, err, expected) in cases {
            assert_eq!(
                client_error_indicates_daemon_unavailable(err),
                *expected,
                "{name} is classified against the documented policy"
            );
        }
    });

    // soldr#1838 Phase 2 / #1837. A wrapper that reaches a daemon in graceful
    // drain must degrade to direct rustc, not fail the build. Before
    // `Retiring` existed this arrived as `Protocol`, which is deliberately
    // classified as NOT unavailable so a genuine daemon bug is never masked --
    // correct for its own case, and exactly wrong for an orderly shutdown.
    timed_test!(a_retiring_daemon_lets_the_wrapper_fall_back, {
        assert!(
            client_error_indicates_daemon_unavailable(&client::ClientError::Retiring),
            "a retiring daemon must permit the direct-rustc fallback"
        );
    });

    // The other half of the boundary: a real protocol violation must still
    // hard-fail. If this ever flips, degrading would hide daemon bugs behind
    // silently uncached builds.
    timed_test!(a_protocol_violation_still_hard_fails, {
        assert!(
            !client_error_indicates_daemon_unavailable(&client::ClientError::Protocol(
                "unexpected frame".into()
            )),
            "a protocol violation must not be treated as daemon-unavailable"
        );
    });

    // soldr#1838 bullet 4. The two stall cases want opposite fixes, so the
    // advice must not be interchangeable: telling someone with a 40-minute LTO
    // link to bypass the cache makes their build slower, and telling someone
    // with a wedged daemon to raise the timeout just prolongs the hang.
    timed_test!(
        a_stall_that_produced_output_is_reported_as_a_slow_compile,
        {
            let text = describe_compile_dispatch_error(&client::ClientError::CompileStalled {
                saw_output: true,
                elapsed: std::time::Duration::from_secs(1800),
            });
            assert!(
                text.contains("1800s"),
                "must name the deadline, got: {text}"
            );
            assert!(
                text.contains("SOLDR_COMPILE_REPLY_TIMEOUT_SECS"),
                "a slow compile should be told how to raise the deadline, got: {text}"
            );
            assert!(
                !text.contains("--no-cache"),
                "bypassing the cache makes a slow compile slower, got: {text}"
            );
        }
    );

    timed_test!(a_silent_stall_is_reported_as_a_wedged_cache, {
        let text = describe_compile_dispatch_error(&client::ClientError::CompileStalled {
            saw_output: false,
            elapsed: std::time::Duration::from_secs(1800),
        });
        assert!(
            text.contains("--no-cache") || text.contains("ZCCACHE_DISABLE"),
            "a wedge should be told how to bypass, got: {text}"
        );
        assert!(
            !text.contains("SOLDR_COMPILE_REPLY_TIMEOUT_SECS"),
            "raising the deadline only prolongs a wedge, got: {text}"
        );
    });

    // Both stall shapes must still permit the fallback -- `saw_output` changes
    // the advice, never whether the wrapper can recover.
    timed_test!(both_stall_shapes_allow_the_direct_rustc_fallback, {
        for saw_output in [true, false] {
            assert!(
                client_error_indicates_daemon_unavailable(&client::ClientError::CompileStalled {
                    saw_output,
                    elapsed: std::time::Duration::from_secs(1),
                }),
                "a stalled compile must degrade (saw_output={saw_output})"
            );
        }
    });

    timed_test!(retiring_is_explained_as_a_shutdown_not_an_error, {
        let text = describe_compile_dispatch_error(&client::ClientError::Retiring);
        assert!(
            text.contains("shutting down"),
            "the message must name the shutdown, got: {text}"
        );
        assert!(
            text.contains("direct rustc"),
            "the message must say what soldr does next, got: {text}"
        );
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
        managed_cache_never_silently_falls_back_to_direct_rustc,
        Duration::from_secs(5),
        {
            let g = EnvVarGuard::acquire(SOLDR_DAEMON_REQUIRED_ENV_VAR);

            // Default: unavailability → fall back.
            assert!(!should_fall_back_to_direct_rustc(&budget_exhausted_with(
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
