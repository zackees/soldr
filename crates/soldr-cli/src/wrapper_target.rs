//! Wrapper hot-path target-registry routing.
//!
//! Issue #474 introduced two routing paths for the per-invocation
//! `target/` registry write the wrapper performs:
//!
//! - **Fast path** — `SOLDR_BUILD_SESSION_ID` is NOT set in the env
//!   (i.e. this rustc invocation didn't come from `soldr cargo …`).
//!   The wrapper writes the row directly to redb and returns. No
//!   daemon IPC, no PID-file probe, no spawn attempt.
//! - **Slow path** — the session id IS set. The wrapper goes through
//!   the daemon (target-touch IPC + per-crate `RecordCompile` event)
//!   and opportunistically auto-spawns the daemon when missing.
//!
//! Lives in its own module so the lib tree exposes it for the
//! integration tests under `tests/cli_wrapper_perf.rs` without
//! dragging the full `wrapper.rs` (which depends on bin-only modules)
//! into the lib's compile.

use crate::core::SoldrPaths;
use std::path::Path;

/// Outcome of a single `record_target_dir_in_registry` call. Used by
/// `wrapper::run_rustc_wrapper` to emit a phase marker matching the
/// path taken, and by the unit tests in this crate to assert that
/// the fast / slow path was selected for the right reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetTouchPath {
    /// No workspace `target/` dir resolvable from the rustc arg list.
    NoTarget,
    /// `SoldrPaths::new()` failed (no `$HOME` / `$USERPROFILE` / cache
    /// dir resolvable).
    NoPaths,
    /// Fast path (Option A of issue #474): no `SOLDR_BUILD_SESSION_ID`
    /// in the environment — direct redb write only.
    FastDirect,
    /// Slow path: session id present, daemon routing engaged.
    DaemonFirst,
}

/// Best-effort upsert of the workspace `target/` dir into the soldr
/// state registry on every wrapper invocation. Silent on failure.
///
/// See module docs for the two-path routing.
pub fn record_target_dir_in_registry(rustc_args: &[String]) -> TargetTouchPath {
    let Some(target) = crate::cache_lib::target_registry::resolve_workspace_target_dir(rustc_args)
    else {
        return TargetTouchPath::NoTarget;
    };
    let Ok(paths) = SoldrPaths::new() else {
        return TargetTouchPath::NoPaths;
    };

    // Fast path: skip the daemon entirely outside a soldr-cargo build.
    let Some(session_id) = read_build_session_id_env() else {
        write_target_direct(&paths, &target);
        return TargetTouchPath::FastDirect;
    };

    // Slow path: in-session — daemon for target-touch + record-compile.
    crate::daemon::client::record_target_touch_or_fallback(&paths, &target);

    let crate_name = parse_crate_name(rustc_args).unwrap_or("unknown");
    let started_at_ms = current_unix_ms();
    crate::daemon::client::record_compile(
        &paths,
        session_id,
        crate_name,
        &target,
        started_at_ms,
        None,
    );

    if crate::daemon::lifecycle::is_live(&paths).is_none() {
        // The spawn itself is serialized via a file lock inside
        // `try_spawn_detached` so N concurrent wrapper invocations
        // don't fork N daemons (see #474 spawn-herd note).
        let _ = crate::daemon::lifecycle::try_spawn_detached();
    }

    TargetTouchPath::DaemonFirst
}

fn write_target_direct(paths: &SoldrPaths, target: &Path) {
    let db_path = crate::cache_lib::data_db_path(paths);
    if let Ok(registry) = crate::cache_lib::target_registry::TargetRegistry::open(&db_path) {
        let _ = registry.upsert(target);
    }
}

pub fn read_build_session_id_env() -> Option<u64> {
    std::env::var(crate::cache_lib::SOLDR_BUILD_SESSION_ID_ENV_VAR)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
}

fn current_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Pull `--crate-name X` (or `--crate-name=X`) from a rustc arg list.
fn parse_crate_name(args: &[String]) -> Option<&str> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--crate-name" {
            return iter.next().map(String::as_str);
        }
        if let Some(value) = arg.strip_prefix("--crate-name=") {
            return Some(value);
        }
    }
    None
}
