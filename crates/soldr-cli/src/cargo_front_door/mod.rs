//! `soldr cargo ...` front door, profile-debug-default detection, linker
//! injection, low-disk warning, and the cargo arg-parsing helpers shared
//! with `rust_plan`. Extracted from `main.rs` as part of issue #339.
//!
//! Split into sub-modules under `cargo_front_door/`:
//! - [`subcommand`] — argv-level cargo subcommand sniffing, cacheability
//!   classification, and target-flag detection.
//! - [`inputs`] — hashing inputs shared with `rust_plan` (profile,
//!   target, feature, env-var, manifest, and config hashes).
//! - [`profile_debug`] — `[profile.<P>].debug` default detection and
//!   the `CARGO_PROFILE_<P>_DEBUG=false` injection / one-shot warning.
//! - [`target`] — target-triple resolution and `SOLDR_LINKER` injection.
//! - [`disk`] — low-disk warning, free-space probing, PATH/arg helpers.
//!
//! This file owns the cross-cutting `run_cargo_front_door` entry, the
//! `--no-gc-target*` flag stripping, the cargo output-capture wrappers,
//! the known-subcommand fetch hook, and the build-session bookkeeping.

use crate::cache_lib::auto_target_gc::{auto_prune_target, render_summary, AutoPrunePhase};
use crate::core::{suppress_windows_console_window, SoldrError, SoldrPaths};
use crate::fetch::VersionSpec;
use crate::trampoline::{
    refresh_sidecar_after_cargo, strip_no_trampoline_flag, try_run_trampoline, TrampolineDecision,
};
use crate::zccache::{
    cache_lifecycle_from_env, command_lifetime_shutdown_timeout, CacheLifecycle,
    SOLDR_CACHE_LIFECYCLE_ENV_VAR, SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS_ENV_VAR,
};
use crate::{
    apply_implicit_toolchain_homes, gc, resolve_toolchain_binary_for_channel, ZccacheSourceArg,
};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use wait_timeout::ChildExt;

mod cache_plan;
mod clang_cl_shim;
mod component_install;
pub(crate) mod cook_hydrate;
mod disk;
mod inputs;
mod no_cache_detach;
mod profile_debug;
mod subcommand;
mod target;
mod zig_shim;

pub(crate) use cache_plan::CargoCachePlan;

const CARGO_WAIT_TIMEOUT_ENV_VAR: &str = "SOLDR_CARGO_WAIT_TIMEOUT_SECS";
/// Internal one-hop marker for commands that must share their Soldr parent's
/// process group. The nested process consumes it before spawning Cargo.
pub(crate) const INHERIT_PARENT_PROCESS_GROUP_ENV: &str =
    "SOLDR_INTERNAL_INHERIT_PROCESS_GROUP";
const CARGO_TIMEOUT_RETRY_DISABLE_ENV_VAR: &str = "SOLDR_NO_CARGO_TIMEOUT_RETRY";
const CARGO_WAIT_HEARTBEAT_SECS: u64 = 60;
const KILLED_CARGO_REAP_TIMEOUT_SECS: u64 = 5;
const CAPTURE_PIPE_EOF_GRACE: Duration = Duration::from_secs(2);
const COMPILE_JOURNAL_TAIL_WAIT: Duration = Duration::from_secs(2);
const COMPILE_JOURNAL_TAIL_POLL: Duration = Duration::from_millis(25);
const BUILD_HISTORY_RETRY_ATTEMPTS: usize = 20;
const BUILD_HISTORY_RETRY_POLL: Duration = Duration::from_millis(25);

// -- Re-exports for cross-module callers --
//
// External modules (`gc`, `rust_plan`, `main`)
// reach into `crate::cargo_front_door::*` using the names that existed
// on the flat file. Re-export them from the sub-modules so the public
// API is byte-for-byte identical after the split.
pub(crate) use disk::{
    available_space, existing_filesystem_probe_path, low_disk_warning_for_free_bytes,
    low_disk_warning_for_path,
};
pub(crate) use inputs::{
    build_env_inputs, cargo_config_hash, cargo_feature_inputs, cargo_profile, cargo_target_triple,
    file_hash_or_missing, path_string, rustflags_inputs, selected_cargo_args, sha256_bytes,
    stable_hash_json, workspace_manifest_hashes,
};
pub(crate) use profile_debug::CargoProfileDebugDefault;
pub(crate) use subcommand::{
    cargo_args_are_cacheable, cargo_args_may_compile_unmediated,
    cargo_args_should_apply_rustfmt_shim, cargo_args_specify_target,
    cargo_args_use_reserved_no_cache, first_cargo_subcommand, first_cargo_subcommand_index,
};

/// 64-bit build session id: high 32 bits = unix-ms truncated, low 32
/// bits = pid-XOR-nanos so two concurrent builds in the same ms never
/// collide. Cheap and good enough for in-process correlation.
fn generate_build_session_id() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let high = ((nanos / 1_000_000) as u64) & 0xFFFF_FFFF;
    let low = ((nanos as u64) ^ (std::process::id() as u64)) & 0xFFFF_FFFF;
    (high << 32) | low
}

struct CargoAbortLogRequest<'a> {
    paths: &'a SoldrPaths,
    session_id: u64,
    repo_root: &'a Path,
    started_at_ms: i64,
    ended_at_ms: i64,
    args: &'a [String],
    timeout: bool,
    cargo_wait_timeout: Option<Duration>,
    cleanup: CargoAbortCleanupReport,
    message: &'a str,
    auto_retry_planned: bool,
}

type CargoRunResult = Result<
    (
        std::process::ExitStatus,
        Option<String>,
        Option<Vec<String>>,
    ),
    SoldrError,
>;

fn append_cargo_abort_log(request: CargoAbortLogRequest<'_>) -> Result<PathBuf, SoldrError> {
    let CargoAbortLogRequest {
        paths,
        session_id,
        repo_root,
        started_at_ms,
        ended_at_ms,
        args,
        timeout,
        cargo_wait_timeout,
        cleanup,
        message,
        auto_retry_planned,
    } = request;
    let path = paths.cargo_abort_log();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let retry_without_cache: Vec<&str> = ["soldr", "--no-cache", "cargo"]
        .into_iter()
        .chain(args.iter().map(String::as_str))
        .collect();
    let retry_with_zccache_disabled: Vec<&str> = ["soldr", "cargo"]
        .into_iter()
        .chain(args.iter().map(String::as_str))
        .collect();
    let record = serde_json::json!({
        "schema_version": 1,
        "event": "cargo_abort",
        "ts_ms": ended_at_ms,
        "session_id": session_id,
        "repo_root": repo_root.display().to_string(),
        "started_at_ms": started_at_ms,
        "ended_at_ms": ended_at_ms,
        "elapsed_ms": (ended_at_ms - started_at_ms).max(0),
        "timeout": timeout,
        "timeout_config": {
            "explicit": cargo_wait_timeout.is_some(),
            "source": cargo_wait_timeout.map(|_| CARGO_WAIT_TIMEOUT_ENV_VAR),
            "duration_secs": cargo_wait_timeout.map(|duration| duration.as_secs()),
        },
        "cargo_args": args,
        "message": message,
        "auto_retry_planned": auto_retry_planned,
        "cleanup": {
            "orphan_rmetas_pruned": cleanup.orphan_rmetas_pruned,
            "incremental_dirs_removed": cleanup.incremental_dirs_removed,
        },
        "recovery": {
            "inspect_logs": ["soldr", "logs", "paths"],
            "retry_without_cache": {
                "argv": retry_without_cache,
            },
            "retry_with_zccache_disabled": {
                "env": { "ZCCACHE_DISABLE": "1" },
                "argv": retry_with_zccache_disabled,
            },
            "clean_hint": ["soldr", "--no-cache", "cargo", "clean", "-p", "<crate>"],
            "timeout_env": {
                "cargo_wait": CARGO_WAIT_TIMEOUT_ENV_VAR,
                "compile_reply": "SOLDR_COMPILE_REPLY_TIMEOUT_SECS",
            },
        },
    });
    let line = serde_json::to_string(&record)
        .map_err(|err| SoldrError::Other(format!("serialize cargo abort log: {err}")))?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(file, "{line}")?;
    Ok(path)
}

fn cargo_timeout_retry_allowed(cache_enabled_for_cargo: bool, args: &[String]) -> bool {
    if !cache_enabled_for_cargo || env_flag_truthy(CARGO_TIMEOUT_RETRY_DISABLE_ENV_VAR) {
        return false;
    }
    matches!(
        first_cargo_subcommand(args),
        Some("b" | "build" | "c" | "check" | "t" | "test" | "clippy" | "d" | "doc")
    )
}

fn retry_timed_out_cargo_without_cache(
    args: &[String],
    explicit_toolchain: Option<&str>,
) -> Result<std::process::ExitStatus, SoldrError> {
    let exe = std::env::current_exe()?;
    let mut command = std::process::Command::new(exe);
    command.arg("--no-cache").arg("cargo").args(args);
    if let Some(toolchain) = explicit_toolchain {
        command.env("RUSTUP_TOOLCHAIN", toolchain);
    }
    command.env(CARGO_TIMEOUT_RETRY_DISABLE_ENV_VAR, "1");
    suppress_windows_console_window(&mut command);
    configure_cargo_child_for_timeout(&mut command);
    let mut child = command
        .spawn()
        .map_err(|err| SoldrError::Other(format!("spawn no-cache cargo retry failed: {err}")))?;
    // The nested soldr invocation inherits the explicit timeout that caused
    // this retry and has recursion disabled above. The outer supervisor must
    // not add a second, implicit deadline of its own.
    wait_for_cargo_child(&mut child, "soldr no-cache cargo retry", None)
}

fn new_build_record(
    session_id: u64,
    repo_root: String,
    started_at_ms: i64,
) -> crate::daemon::protocol::BuildRecord {
    crate::daemon::protocol::BuildRecord {
        session_id,
        repo_root,
        started_at_ms,
        ended_at_ms: None,
        exit_code: None,
        total_wall_ms: None,
        crate_count: 0,
        slowest_crate_us: None,
        slowest_crate_name: None,
        cache_summary: None,
        log_paths: None,
        miss_reasons: Vec::new(),
    }
}

fn persist_build_session_start_fallback(
    paths: &SoldrPaths,
    session_id: u64,
    repo_root: &Path,
    started_at_ms: i64,
) {
    if let Err(err) =
        persist_build_session_start_fallback_inner(paths, session_id, repo_root, started_at_ms)
    {
        eprintln!(
            "soldr warning: failed to persist build-session start fallback for {session_id}: {err}"
        );
    }
}

fn persist_build_session_start_fallback_inner(
    paths: &SoldrPaths,
    session_id: u64,
    repo_root: &Path,
    started_at_ms: i64,
) -> Result<(), SoldrError> {
    let db_path = crate::cache_lib::data_db_path(paths);
    if crate::daemon::db::get_build(&db_path, session_id)
        .map_err(|e| SoldrError::Other(format!("read build history: {e}")))?
        .is_none()
    {
        let record = new_build_record(session_id, repo_root.display().to_string(), started_at_ms);
        crate::daemon::db::upsert_build(&db_path, &record)
            .map_err(|e| SoldrError::Other(format!("write build history: {e}")))?;
    }
    let _ = crate::daemon::db::append_event(
        &db_path,
        &crate::daemon::db::Event {
            ts_ms: started_at_ms,
            session_id: Some(session_id),
            kind: crate::daemon::db::EventKind::SessionStart,
            crate_name: None,
            duration_us: None,
            target_dir: None,
            exit_code: None,
        },
    );
    Ok(())
}

fn persist_build_session_end_fallback(
    paths: &SoldrPaths,
    session_id: u64,
    exit_code: i32,
    ended_at_ms: i64,
) {
    if let Err(err) =
        persist_build_session_end_fallback_inner(paths, session_id, exit_code, ended_at_ms)
    {
        eprintln!(
            "soldr warning: failed to persist build-session end fallback for {session_id}: {err}"
        );
    }
}

fn persist_build_session_end_fallback_inner(
    paths: &SoldrPaths,
    session_id: u64,
    exit_code: i32,
    ended_at_ms: i64,
) -> Result<(), SoldrError> {
    let db_path = crate::cache_lib::data_db_path(paths);
    let mut record = crate::daemon::db::get_build(&db_path, session_id)
        .map_err(|e| SoldrError::Other(format!("read build history: {e}")))?
        .unwrap_or_else(|| {
            let repo_root = std::env::current_dir()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|_| ".".to_string());
            new_build_record(session_id, repo_root, ended_at_ms)
        });
    let (crate_count, slowest_crate_us, slowest_crate_name) =
        crate::daemon::db::aggregate_session(&db_path, session_id).unwrap_or((0, None, None));
    record.ended_at_ms = Some(ended_at_ms);
    record.exit_code = Some(exit_code);
    record.total_wall_ms = Some((ended_at_ms - record.started_at_ms).max(0) as u64);
    record.crate_count = crate_count;
    record.slowest_crate_us = slowest_crate_us;
    record.slowest_crate_name = slowest_crate_name;
    crate::daemon::db::upsert_build(&db_path, &record)
        .map_err(|e| SoldrError::Other(format!("write build history: {e}")))?;
    let _ = crate::daemon::db::append_event(
        &db_path,
        &crate::daemon::db::Event {
            ts_ms: ended_at_ms,
            session_id: Some(session_id),
            kind: crate::daemon::db::EventKind::SessionEnd,
            crate_name: None,
            duration_us: None,
            target_dir: None,
            exit_code: Some(exit_code),
        },
    );
    Ok(())
}

#[derive(Clone, Copy)]
struct BuildLogHistoryRequest<'a> {
    paths: &'a SoldrPaths,
    build_session_id: u64,
    repo_root: &'a Path,
    started_at_ms: i64,
    session: &'a crate::zccache_lifecycle::ZccacheBuildSession,
    compile_journal_start_len: u64,
    exit_code: i32,
    ended_at_ms: i64,
    /// soldr#1536: true when the daemon acknowledged `BuildSessionEnd`,
    /// meaning the persisted BuildRecord already carries the finalized
    /// crate-count / slowest-crate aggregate and every session event is
    /// durable — the wrapper must NOT redo the O(all-history)
    /// `aggregate_session` scan in that case.
    daemon_finalized: bool,
}

fn persist_build_log_history(request: BuildLogHistoryRequest<'_>) {
    let build_session_id = request.build_session_id;
    let mut last_error = None;
    for attempt in 0..BUILD_HISTORY_RETRY_ATTEMPTS {
        match persist_build_log_history_inner(&request) {
            Ok(()) => return,
            Err(err) => {
                last_error = Some(err);
                if attempt + 1 < BUILD_HISTORY_RETRY_ATTEMPTS {
                    std::thread::sleep(BUILD_HISTORY_RETRY_POLL);
                }
            }
        }
    }
    if let Some(err) = last_error {
        eprintln!(
            "soldr warning: failed to persist logs history for build {build_session_id}: {err}"
        );
    }
}

fn persist_build_log_history_inner(request: &BuildLogHistoryRequest<'_>) -> Result<(), SoldrError> {
    let BuildLogHistoryRequest {
        paths,
        build_session_id,
        repo_root,
        started_at_ms,
        session,
        compile_journal_start_len,
        exit_code,
        ended_at_ms,
        daemon_finalized,
    } = *request;
    let db_path = crate::cache_lib::data_db_path(paths);
    let mut record = crate::daemon::db::get_build(&db_path, build_session_id)
        .map_err(|e| SoldrError::Other(format!("read build history: {e}")))?
        .unwrap_or_else(|| {
            new_build_record(
                build_session_id,
                repo_root.display().to_string(),
                started_at_ms,
            )
        });

    let archive_dir = build_log_history_dir(paths, build_session_id);
    let archived_session_log_path =
        copy_session_artifact(&session.session_log_path, &archive_dir, "last-session.log");
    let archived_journal_path =
        copy_session_artifact(&session.journal_path, &archive_dir, "last-session.jsonl");
    let archived_session_stats_path = copy_session_artifact(
        &session.session_stats_path,
        &archive_dir,
        "last-session-stats.json",
    );
    let cache_summary = read_build_cache_summary(&session.session_stats_path);
    let expected_compile_journal_entries = cache_summary
        .as_ref()
        .and_then(|summary| (summary.compilations > 0).then_some(summary.compilations));
    let compile_journal_path = embedded_compile_journal_path(paths);
    if let Some(expected) = expected_compile_journal_entries {
        wait_for_compile_journal_tail(&compile_journal_path, compile_journal_start_len, expected);
    }
    let archived_compile_journal_path = copy_session_artifact_tail(
        &compile_journal_path,
        &archive_dir,
        "compile_journal.jsonl",
        compile_journal_start_len,
    );

    record.cache_summary = cache_summary;
    record.miss_reasons = read_build_miss_reasons(
        archived_compile_journal_path
            .as_ref()
            .map(|path| Path::new(path.as_str())),
        archived_journal_path
            .as_ref()
            .map(|path| Path::new(path.as_str()))
            .unwrap_or(&session.journal_path),
        archived_session_log_path
            .as_ref()
            .map(|path| Path::new(path.as_str()))
            .unwrap_or(&session.session_log_path),
    );
    // soldr#1536: when the daemon acknowledged BuildSessionEnd, the
    // record read above already carries the finalized crate-count /
    // slowest-crate aggregate — keep it. Only the daemon-unreachable
    // fallback still derives the aggregate from the event table.
    if !daemon_finalized {
        let (crate_count, slowest_crate_us, slowest_crate_name) =
            crate::daemon::db::aggregate_session(&db_path, build_session_id)
                .unwrap_or((0, None, None));
        record.crate_count = crate_count;
        record.slowest_crate_us = slowest_crate_us;
        record.slowest_crate_name = slowest_crate_name;
    }
    record.ended_at_ms = Some(record.ended_at_ms.unwrap_or(ended_at_ms));
    record.exit_code = Some(record.exit_code.unwrap_or(exit_code));
    record.total_wall_ms = Some(
        record
            .ended_at_ms
            .map(|ended| (ended - record.started_at_ms).max(0) as u64)
            .unwrap_or(0),
    );
    record.log_paths = Some(crate::daemon::protocol::BuildLogPaths {
        zccache_session_id: Some(session.session_id.clone()),
        cache_dir: Some(session.cache_dir.display().to_string()),
        session_log_path: Some(session.session_log_path.display().to_string()),
        journal_path: Some(session.journal_path.display().to_string()),
        session_stats_path: Some(session.session_stats_path.display().to_string()),
        compile_journal_path: Some(compile_journal_path.display().to_string()),
        archived_session_log_path,
        archived_journal_path,
        archived_session_stats_path,
        archived_compile_journal_path,
        // soldr#1368: private managed-zccache daemons are gone; the
        // field stays on the wire for older records.
        private_daemon_name: None,
    });

    crate::daemon::db::upsert_build(&db_path, &record)
        .map_err(|e| SoldrError::Other(format!("write build history: {e}")))?;
    Ok(())
}

fn build_log_history_dir(paths: &SoldrPaths, build_session_id: u64) -> PathBuf {
    paths
        .cache
        .join("zccache")
        .join("history")
        .join(build_session_id.to_string())
}

fn embedded_compile_journal_path(paths: &SoldrPaths) -> PathBuf {
    paths
        .cache
        .join("zccache")
        .join(format!("v{}", zccache::core::VERSION))
        .join("logs")
        .join("compile_journal.jsonl")
}

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}

/// Wait for the embedded compile journal to contain the expected number
/// of entries for this build.
///
/// soldr#1536: the pre-#1536 version demanded three consecutive 25 ms
/// "stable length" polls even when the journal was already complete,
/// putting a fixed ~75 ms floor on every finalization. Completeness is
/// now judged directly: an entry only counts once its line is
/// newline-terminated (the zccache journal thread writes whole lines),
/// so as soon as `expected_entries` complete lines are visible the wait
/// returns without sleeping at all — the common case, since the journal
/// entries were enqueued before the last compile reply and the daemon
/// ack round-trip already happened. The 2 s deadline only bounds the
/// rare case where the journal writer thread lags.
fn wait_for_compile_journal_tail(path: &Path, start_offset: u64, expected_entries: u64) -> bool {
    wait_for_compile_journal_tail_with(
        path,
        start_offset,
        expected_entries,
        COMPILE_JOURNAL_TAIL_WAIT,
        || std::thread::sleep(COMPILE_JOURNAL_TAIL_POLL),
    )
}

/// Testable core of [`wait_for_compile_journal_tail`] with an injected
/// sleep so tests can assert the zero-sleep fast path.
fn wait_for_compile_journal_tail_with(
    path: &Path,
    start_offset: u64,
    expected_entries: u64,
    wait_budget: Duration,
    mut sleep: impl FnMut(),
) -> bool {
    let deadline = Instant::now() + wait_budget;
    loop {
        if expected_entries == 0
            || count_complete_compile_journal_tail_entries(path, start_offset).unwrap_or(0)
                >= expected_entries
        {
            return true;
        }
        if Instant::now() >= deadline {
            // Best effort past the deadline: report whether ANY tail
            // showed up so the caller still archives what exists.
            return file_len(path) > start_offset;
        }
        sleep();
    }
}

/// Count newline-terminated, non-empty journal lines past `start_offset`.
/// A trailing line without its `\n` is still in flight (partial write by
/// the journal thread or a concurrent build) and does not count.
fn count_complete_compile_journal_tail_entries(path: &Path, start_offset: u64) -> Option<u64> {
    let tail = read_file_tail(path, start_offset)?;
    Some(
        tail.split_inclusive('\n')
            .filter(|line| line.ends_with('\n') && !line.trim().is_empty())
            .count() as u64,
    )
}

fn read_file_tail(path: &Path, start_offset: u64) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len <= start_offset {
        return None;
    }
    file.seek(SeekFrom::Start(start_offset)).ok()?;
    let mut body = String::new();
    file.read_to_string(&mut body).ok()?;
    if body.is_empty() {
        None
    } else {
        Some(body)
    }
}

fn copy_session_artifact(source: &Path, archive_dir: &Path, file_name: &str) -> Option<String> {
    if !source.is_file() {
        return None;
    }
    std::fs::create_dir_all(archive_dir).ok()?;
    let dest = archive_dir.join(file_name);
    std::fs::copy(source, &dest).ok()?;
    Some(dest.display().to_string())
}

fn copy_session_artifact_tail(
    source: &Path,
    archive_dir: &Path,
    file_name: &str,
    start_offset: u64,
) -> Option<String> {
    let tail = read_file_tail(source, start_offset)?;
    // soldr#1536: drop a trailing partial line (an in-flight write by
    // the journal thread or a concurrent build) so the archive holds
    // only complete JSONL records. A tail with no newline at all is
    // kept whole — better a truncated best-effort record than nothing.
    let complete = match tail.rfind('\n') {
        Some(last_newline) => &tail[..=last_newline],
        None => tail.as_str(),
    };
    std::fs::create_dir_all(archive_dir).ok()?;
    let dest = archive_dir.join(file_name);
    std::fs::write(&dest, complete).ok()?;
    Some(dest.display().to_string())
}

fn read_build_cache_summary(
    stats_path: &Path,
) -> Option<crate::daemon::protocol::BuildCacheSummary> {
    let raw = std::fs::read_to_string(stats_path).ok()?;
    let json: serde_json::Value = serde_json::from_str(raw.trim()).ok()?;
    if json.get("status").and_then(serde_json::Value::as_str) != Some("ok") {
        return None;
    }
    let hits = json_u64(&json, "hits").unwrap_or(0);
    let misses = json_u64(&json, "misses").unwrap_or(0);
    let non_cacheable = json_u64(&json, "non_cacheable").unwrap_or(0);
    let errors = json_u64(&json, "errors").unwrap_or(0);
    Some(crate::daemon::protocol::BuildCacheSummary {
        hits,
        misses,
        non_cacheable,
        errors,
        compilations: json_u64(&json, "compilations").unwrap_or(hits + misses),
        time_saved_ms: json_u64(&json, "time_saved_ms").unwrap_or(0),
    })
}

fn read_build_miss_reasons(
    compile_journal_path: Option<&Path>,
    session_journal_path: &Path,
    session_log_path: &Path,
) -> Vec<crate::daemon::protocol::BuildMissReason> {
    if let Some(compile_journal_path) = compile_journal_path {
        let from_compile_journal = read_build_miss_reasons_from_journal(compile_journal_path);
        if !from_compile_journal.is_empty() {
            return from_compile_journal;
        }
    }
    let from_session_journal = read_build_miss_reasons_from_journal(session_journal_path);
    if !from_session_journal.is_empty() {
        return from_session_journal;
    }
    read_build_miss_reasons_from_log(session_log_path)
}

fn read_build_miss_reasons_from_journal(
    journal_path: &Path,
) -> Vec<crate::daemon::protocol::BuildMissReason> {
    let Ok(raw) = std::fs::read_to_string(journal_path) else {
        return Vec::new();
    };
    parse_build_miss_reasons_from_journal(&raw)
}

fn parse_build_miss_reasons_from_journal(
    journal_body: &str,
) -> Vec<crate::daemon::protocol::BuildMissReason> {
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    for line in journal_body.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let outcome = value
            .get("outcome")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if !matches!(outcome, "miss" | "link_miss") {
            continue;
        }
        let reason = value
            .get("miss_reason")
            .and_then(serde_json::Value::as_str)
            .filter(|reason| !reason.trim().is_empty())
            .unwrap_or("unknown")
            .to_string();
        *counts.entry(reason).or_insert(0) += 1;
    }
    sorted_miss_reasons(counts)
}

fn read_build_miss_reasons_from_log(
    log_path: &Path,
) -> Vec<crate::daemon::protocol::BuildMissReason> {
    let Ok(raw) = std::fs::read_to_string(log_path) else {
        return Vec::new();
    };
    parse_build_miss_reasons_from_log(&raw)
}

fn parse_build_miss_reasons_from_log(
    log_body: &str,
) -> Vec<crate::daemon::protocol::BuildMissReason> {
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    for line in log_body.lines().filter(|line| line.contains("[MISS]")) {
        let reason = extract_miss_reason(line).unwrap_or_else(|| "unknown".to_string());
        *counts.entry(reason).or_insert(0) += 1;
    }
    if counts.is_empty() {
        for line in log_body
            .lines()
            .filter(|line| line.contains("verdict=Miss"))
        {
            let reason = extract_miss_reason(line).unwrap_or_else(|| "unknown".to_string());
            *counts.entry(reason).or_insert(0) += 1;
        }
    }
    sorted_miss_reasons(counts)
}

fn sorted_miss_reasons(
    counts: BTreeMap<String, u64>,
) -> Vec<crate::daemon::protocol::BuildMissReason> {
    let mut reasons: Vec<_> = counts
        .into_iter()
        .map(|(reason, count)| crate::daemon::protocol::BuildMissReason { reason, count })
        .collect();
    reasons.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.reason.cmp(&b.reason)));
    reasons
}

fn extract_miss_reason(line: &str) -> Option<String> {
    if let Some(rest) = line.split("(reason:").nth(1) {
        let reason = rest.split(')').next()?.trim();
        if !reason.is_empty() {
            return Some(reason.to_string());
        }
    }
    if let Some(rest) = line.split("reason=").nth(1) {
        let reason = rest
            .split_whitespace()
            .next()?
            .trim_matches(|c: char| matches!(c, ',' | ';' | ')' | ']'))
            .trim();
        if !reason.is_empty() {
            return Some(reason.to_string());
        }
    }
    None
}

fn json_u64(value: &serde_json::Value, key: &str) -> Option<u64> {
    value.get(key).and_then(serde_json::Value::as_u64)
}

fn cargo_wait_timeout() -> Result<Option<Duration>, SoldrError> {
    let value = match std::env::var(CARGO_WAIT_TIMEOUT_ENV_VAR) {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => return Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(SoldrError::Other(format!(
                "invalid {CARGO_WAIT_TIMEOUT_ENV_VAR}: expected 0 or a positive integer number of seconds, but the value is not valid Unicode"
            )))
        }
    };
    let seconds = value.parse::<u64>().map_err(|_| {
        SoldrError::Other(format!(
            "invalid {CARGO_WAIT_TIMEOUT_ENV_VAR}={value:?}: expected 0 or a positive integer number of seconds"
        ))
    })?;
    Ok((seconds > 0).then(|| Duration::from_secs(seconds)))
}

fn wait_for_cargo_child(
    child: &mut std::process::Child,
    context: &str,
    timeout: Option<Duration>,
) -> Result<std::process::ExitStatus, SoldrError> {
    wait_for_cargo_child_with_heartbeat(
        child,
        context,
        timeout,
        Duration::from_secs(CARGO_WAIT_HEARTBEAT_SECS),
    )
}

fn wait_for_cargo_child_with_heartbeat(
    child: &mut std::process::Child,
    context: &str,
    timeout: Option<Duration>,
    heartbeat: Duration,
) -> Result<std::process::ExitStatus, SoldrError> {
    let start = Instant::now();
    loop {
        let elapsed = start.elapsed();
        if let Some(timeout) = timeout {
            if elapsed >= timeout {
                return Err(cargo_timeout_error(child, context, timeout));
            }
        }
        let wait_for = timeout
            .map(|timeout| timeout.saturating_sub(elapsed).min(heartbeat))
            .unwrap_or(heartbeat);
        match child
            .wait_timeout(wait_for)
            .map_err(|err| SoldrError::Other(format!("wait on {context} failed: {err}")))?
        {
            Some(status) => return Ok(status),
            None => {
                if let Some(timeout) = timeout {
                    if start.elapsed() >= timeout {
                        return Err(cargo_timeout_error(child, context, timeout));
                    }
                }
                eprintln!(
                    "{}",
                    cargo_wait_heartbeat_message(context, start.elapsed(), timeout)
                );
            }
        }
    }
}

fn cargo_wait_heartbeat_message(
    context: &str,
    elapsed: Duration,
    timeout: Option<Duration>,
) -> String {
    match timeout {
        Some(timeout) => format!(
            "soldr: {context} still running after {}s (explicit timeout {}s from {CARGO_WAIT_TIMEOUT_ENV_VAR})",
            elapsed.as_secs(),
            timeout.as_secs()
        ),
        None => format!(
            "soldr: {context} still running after {}s (no wall-clock deadline configured)",
            elapsed.as_secs()
        ),
    }
}

fn cargo_timeout_error(
    child: &mut std::process::Child,
    context: &str,
    timeout: Duration,
) -> SoldrError {
    let kill_result = kill_cargo_process_tree(child);
    let reap_result = child.wait_timeout(Duration::from_secs(KILLED_CARGO_REAP_TIMEOUT_SECS));
    let timeout_secs = timeout.as_secs();
    let mut message = format!(
        "{context} timed out after {timeout_secs} seconds \
         (explicitly configured by {CARGO_WAIT_TIMEOUT_ENV_VAR}; set it to 0 to disable)"
    );
    match kill_result {
        Ok(detail) => message.push_str(&format!("; {detail}")),
        Err(err) => message.push_str(&format!("; kill failed: {err}")),
    }
    match reap_result {
        Ok(Some(_)) => {}
        Ok(None) => message.push_str(&format!(
            "; process did not exit within {KILLED_CARGO_REAP_TIMEOUT_SECS} seconds after kill"
        )),
        Err(err) => message.push_str(&format!("; reap after kill failed: {err}")),
    }
    SoldrError::Other(message)
}

#[cfg(windows)]
pub(crate) fn kill_cargo_process_tree(
    child: &mut std::process::Child,
) -> std::io::Result<&'static str> {
    let pid = child.id().to_string();
    let taskkill = std::process::Command::new("taskkill")
        .args(["/PID", &pid, "/T", "/F"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match taskkill {
        Ok(status) if status.success() => Ok("killed child process tree"),
        _ => {
            child.kill()?;
            Ok("killed child process")
        }
    }
}

#[cfg(unix)]
pub(crate) fn kill_cargo_process_tree(
    child: &mut std::process::Child,
) -> std::io::Result<&'static str> {
    let pgid = child.id() as libc::pid_t;
    let term_result = signal_process_group(pgid, libc::SIGTERM);
    std::thread::sleep(Duration::from_millis(100));
    let kill_result = signal_process_group(pgid, libc::SIGKILL);
    if term_result.is_ok() || kill_result.is_ok() {
        return Ok("signaled cargo process group");
    }
    child.kill()?;
    Ok("killed child process")
}

#[cfg(unix)]
fn signal_process_group(pgid: libc::pid_t, signal: libc::c_int) -> std::io::Result<()> {
    // SAFETY: `pgid` is the spawned cargo child's PID after `process_group(0)`;
    // negating it asks the kernel to signal that process group.
    let rc = unsafe { libc::kill(-pgid, signal) };
    if rc == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(err)
}

#[cfg(all(not(windows), not(unix)))]
pub(crate) fn kill_cargo_process_tree(
    child: &mut std::process::Child,
) -> std::io::Result<&'static str> {
    child.kill()?;
    Ok("killed child process")
}

pub(crate) fn configure_cargo_child_for_timeout(command: &mut std::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        if std::env::var_os(INHERIT_PARENT_PROCESS_GROUP_ENV).is_none() {
            command.process_group(0);
        } else {
            command.env_remove(INHERIT_PARENT_PROCESS_GROUP_ENV);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = command;
    }
}

fn current_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Soldr-private opt-out flags for the auto target-GC hooks (#485).
/// Stripped from the arg vector before forwarding to cargo, since
/// cargo doesn't understand them.
pub(crate) const NO_GC_TARGET_FLAG: &str = "--no-gc-target";
pub(crate) const NO_GC_TARGET_BEFORE_FLAG: &str = "--no-gc-target-before";
pub(crate) const NO_GC_TARGET_AFTER_FLAG: &str = "--no-gc-target-after";
/// Env-var fallback for the wrapper-side path where cargo can't
/// forward flags to soldr. Treated as equivalent to `--no-gc-target`
/// when set to a non-empty value.
pub(crate) const NO_GC_TARGET_ENV_VAR: &str = "SOLDR_NO_GC_TARGET";

const INHERITED_SOLDR_WORKSPACE_ENV_VARS: &[&str] = &[
    crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR,
    crate::cache_lib::MANAGED_ZCCACHE_CACHE_DIR_ENV_VAR,
    crate::cache_lib::ZCCACHE_SESSION_ID_ENV_VAR,
    crate::wrapper_target::TARGET_REGISTRY_RECORDED_ENV_VAR,
    crate::TARGET_CACHE_MODE_ENV_VAR,
    "SOLDR_TARGET_CACHE_DIR",
    crate::TARGET_CACHE_BUNDLE_DIR_ENV_VAR,
    crate::TARGET_CACHE_PROFILE_ENV_VAR,
    crate::TARGET_CACHE_BACKEND_ENV_VAR,
    crate::TARGET_CACHE_TAR_THREADS_ENV_VAR,
    "SOLDR_TARGET_CACHE_COMPRESS",
    "SOLDR_TARGET_CACHE_COMPRESS_LEVEL",
    "SOLDR_BUILD_CACHE_MODE",
];

struct EnvRestore {
    key: OsString,
    previous: Option<OsString>,
}

struct FreshSoldrWorkspaceEnvGuard {
    entries: Vec<EnvRestore>,
}

impl FreshSoldrWorkspaceEnvGuard {
    fn apply_unless_trusted(trust_inherited_soldr_env: bool) -> Self {
        if trust_inherited_soldr_env {
            return Self {
                entries: Vec::new(),
            };
        }

        let mut keys: Vec<OsString> = INHERITED_SOLDR_WORKSPACE_ENV_VARS
            .iter()
            .map(OsString::from)
            .collect();
        keys.extend(
            std::env::vars_os()
                .map(|(key, _)| key)
                .filter(|key| key.to_string_lossy().starts_with("SETUP_SOLDR_")),
        );
        keys.sort();
        keys.dedup();

        let mut entries = Vec::new();
        for key in keys {
            let previous = std::env::var_os(&key);
            if previous.is_some() {
                std::env::remove_var(&key);
                entries.push(EnvRestore { key, previous });
            }
        }
        Self { entries }
    }
}

impl Drop for FreshSoldrWorkspaceEnvGuard {
    fn drop(&mut self) {
        for entry in self.entries.iter().rev() {
            if let Some(value) = &entry.previous {
                std::env::set_var(&entry.key, value);
            }
        }
    }
}

/// Outcome of stripping the `--no-gc-target*` flags from a cargo arg
/// vector. Mirrors the env-var fallback so callers can union all
/// inputs into a single before/after decision.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct GcTargetOptOut {
    pub before: bool,
    pub after: bool,
}

impl GcTargetOptOut {
    fn merged_with_env(mut self) -> Self {
        if env_disables_target_gc() {
            self.before = true;
            self.after = true;
        }
        self
    }
}

fn env_disables_target_gc() -> bool {
    std::env::var_os(NO_GC_TARGET_ENV_VAR)
        .map(|v| {
            let s = v.to_string_lossy();
            let t = s.trim();
            !t.is_empty() && !t.eq_ignore_ascii_case("0") && !t.eq_ignore_ascii_case("false")
        })
        .unwrap_or(false)
}

fn env_flag_truthy(key: &str) -> bool {
    std::env::var_os(key)
        .map(|v| {
            let s = v.to_string_lossy();
            let t = s.trim();
            !t.is_empty() && !t.eq_ignore_ascii_case("0") && !t.eq_ignore_ascii_case("false")
        })
        .unwrap_or(false)
}

/// Issue #1364: a truthy `ZCCACHE_DISABLE` in the caller's environment is
/// treated as `--no-cache`.
///
/// `ZCCACHE_DISABLE` is the standard zccache kill-switch, but soldr never
/// consulted it, so users who set it saw no effect (the build still went
/// through the wrapper/daemon). Mapping it onto the existing `--no-cache`
/// path fully bypasses the wrapper + daemon (and propagates
/// `SOLDR_CACHE_ENABLED=0` to the child cargo), which is also the
/// recovery path when a build hangs on a wedged cache.
pub(crate) fn zccache_disable_requested() -> bool {
    env_flag_truthy("ZCCACHE_DISABLE")
}

/// Remove soldr-private `--no-gc-target*` flags from the arg vector and
/// return the cleaned slice plus which passes the caller asked to skip.
/// Flags after the `--` separator are passed through untouched.
pub(crate) fn strip_no_gc_target_flags(args: &[String]) -> (Vec<String>, GcTargetOptOut) {
    let mut cleaned = Vec::with_capacity(args.len());
    let mut opt_out = GcTargetOptOut::default();
    let mut past_separator = false;
    for arg in args {
        if past_separator {
            cleaned.push(arg.clone());
            continue;
        }
        if arg == "--" {
            past_separator = true;
            cleaned.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            NO_GC_TARGET_FLAG => {
                opt_out.before = true;
                opt_out.after = true;
            }
            NO_GC_TARGET_BEFORE_FLAG => opt_out.before = true,
            NO_GC_TARGET_AFTER_FLAG => opt_out.after = true,
            _ => cleaned.push(arg.clone()),
        }
    }
    (cleaned, opt_out)
}

/// Resolve the cargo `target/` directory that an auto-prune pass should
/// operate on. Mirrors cargo's resolution order:
/// 1. `--target-dir <DIR>` inside the arg list.
/// 2. `CARGO_TARGET_DIR` env var (if non-empty).
/// 3. `<workspace_root>/target` derived from the nearest enclosing
///    `Cargo.toml` to cwd.
///
/// Returns `None` when no manifest can be found cheaply — the auto-hook
/// silently skips in that case rather than guessing.
fn resolve_target_dir_for_gc(args: &[String]) -> Option<std::path::PathBuf> {
    if let Some(value) = disk::cargo_arg_value(args, "--target-dir") {
        return Some(disk::absolutize_path(std::path::PathBuf::from(value)));
    }
    if let Some(env_dir) = std::env::var_os("CARGO_TARGET_DIR") {
        let s = env_dir.to_string_lossy().trim().to_string();
        if !s.is_empty() {
            return Some(disk::absolutize_path(std::path::PathBuf::from(s)));
        }
    }
    let manifest = crate::trampoline::find_nearest_manifest()?;
    let manifest_dir = manifest.parent()?.to_path_buf();
    Some(manifest_dir.join("target"))
}

fn apply_target_registry_memo(
    command: &mut std::process::Command,
    target_dir: &std::path::Path,
    paths: &SoldrPaths,
) {
    // `cargo clean` removes target/ before the next soldr invocation. The
    // future path is still authoritative and the registry accepts paths that
    // do not exist yet, so absence must not disable wrapper memoization.
    let recorded = canonicalize_future_path(target_dir);
    let db_path = crate::cache_lib::data_db_path(paths);
    if let Ok(registry) = crate::cache_lib::target_registry::TargetRegistry::open(&db_path) {
        let _ = registry.upsert(&recorded);
    }
    command.env(
        crate::wrapper_target::TARGET_REGISTRY_RECORDED_ENV_VAR,
        recorded.as_os_str(),
    );
}

fn canonicalize_future_path(path: &std::path::Path) -> std::path::PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }

    let mut missing = Vec::new();
    let mut ancestor = path;
    while let Some(name) = ancestor.file_name() {
        missing.push(name.to_os_string());
        let Some(parent) = ancestor.parent() else {
            break;
        };
        if let Ok(mut canonical) = std::fs::canonicalize(parent) {
            for component in missing.iter().rev() {
                canonical.push(component);
            }
            return canonical;
        }
        ancestor = parent;
    }

    path.to_path_buf()
}

fn emit_auto_prune_summary(outcome: &crate::cache_lib::auto_target_gc::AutoPruneOutcome) {
    if let Some(line) = render_summary(outcome) {
        eprintln!("{line}");
    }
}

/// Pre-cargo target-GC pass (#485), made restore-aware for issue #1558.
///
/// The keep-latest prune ranks hash families by recency and keeps only
/// one family per artifact prefix. A rust-plan restore into a fresh
/// `target/` legitimately materializes multiple live hash families per
/// prefix (build-dependency vs. normal-dependency variants of the same
/// crate, differing feature unification), all carrying the bundle's
/// preserved timestamps. Running the destructive pass between the
/// restore and the cargo launch therefore discarded families Cargo was
/// about to declare Fresh, converting a correct restore into a wave of
/// `Compiling` units ("target GC pruning 64 restored hash families").
///
/// When the immediately preceding verified restore materialized at
/// least one file, the pass is skipped for this invocation only:
/// * The protection is keyed to the verified plan — the restore itself
///   validated toolchain/target/profile/inputs via the plan cache key,
///   and it only runs into a target with zero populated `.fingerprint/`
///   dirs (the #480 guard), so the tree content IS the verified bundle.
/// * It expires conservatively — nothing is persisted; the post-build
///   GC pass still runs unconditionally, after Cargo has re-established
///   authoritative `invoked.timestamp` recency, so genuinely stale
///   families are still pruned in the same build.
/// * Unknown or partial state (no plan, skipped restore, zero files
///   restored, restore error) falls back to today's GC behavior.
///
/// Returns `None` when the pass was skipped, otherwise the prune
/// outcome for the caller to render.
pub(crate) fn run_pre_cargo_target_gc(
    target_dir: &std::path::Path,
    restore_outcome: &crate::rust_plan::RustPlanRestoreOutcome,
) -> Option<crate::cache_lib::auto_target_gc::AutoPruneOutcome> {
    if let Some(restored) = restore_outcome.materialized_file_count() {
        eprintln!(
            "soldr: target-gc (before): skipped for {}; rust-plan restore just \
             materialized {restored} file(s) — deferring to the post-build pass so \
             cargo evaluates the restored hash families first (#1558)",
            target_dir.display()
        );
        return None;
    }
    Some(auto_prune_target(target_dir, AutoPrunePhase::Before))
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct CargoAbortCleanupReport {
    orphan_rmetas_pruned: usize,
    incremental_dirs_removed: usize,
}

impl CargoAbortCleanupReport {
    fn summary(self) -> String {
        format!(
            "pruned {} orphan .rmeta file(s), removed {} incremental/ dir(s)",
            self.orphan_rmetas_pruned, self.incremental_dirs_removed
        )
    }
}

fn cargo_run_error_is_timeout(err: &SoldrError) -> bool {
    matches!(err, SoldrError::Other(message) if message.contains(CARGO_WAIT_TIMEOUT_ENV_VAR) && message.contains("timed out after"))
}

fn cleanup_after_aborted_cargo_run(
    cache_plan: &CargoCachePlan,
    args: &[String],
    timeout: bool,
) -> CargoAbortCleanupReport {
    let orphan_rmetas_pruned = cache_plan.prune_orphan_rmetas_after_failed_build();
    let incremental_dirs_removed = if timeout {
        cache_plan
            .target_dir_for_hooks(args)
            .as_deref()
            .map(cleanup_target_incremental_dirs_after_aborted_build)
            .unwrap_or(0)
    } else {
        0
    };
    if orphan_rmetas_pruned > 0 || incremental_dirs_removed > 0 {
        eprintln!(
            "soldr: cleanup after aborted cargo build: {} (soldr#1384)",
            CargoAbortCleanupReport {
                orphan_rmetas_pruned,
                incremental_dirs_removed,
            }
            .summary()
        );
    }
    CargoAbortCleanupReport {
        orphan_rmetas_pruned,
        incremental_dirs_removed,
    }
}

fn cleanup_target_incremental_dirs_after_aborted_build(target_dir: &std::path::Path) -> usize {
    let mut candidates = Vec::new();
    collect_incremental_dir_candidates(target_dir, &mut candidates);
    candidates.sort();
    candidates.dedup();

    let mut removed = 0usize;
    for path in candidates {
        if !path.is_dir() {
            continue;
        }
        match std::fs::remove_dir_all(&path) {
            Ok(()) => removed = removed.saturating_add(1),
            Err(err) => eprintln!(
                "soldr warning: failed to remove incremental dir {} after aborted cargo build: {err}",
                path.display()
            ),
        }
    }
    removed
}

fn collect_incremental_dir_candidates(
    target_dir: &std::path::Path,
    out: &mut Vec<std::path::PathBuf>,
) {
    let Ok(first_level) = std::fs::read_dir(target_dir) else {
        return;
    };
    for first in first_level.flatten() {
        let first_path = first.path();
        if !first_path.is_dir() {
            continue;
        }
        let direct = first_path.join(crate::cache_lib::prune_target::INCREMENTAL_SUBDIR);
        if direct.is_dir() {
            out.push(direct);
        }
        let Ok(second_level) = std::fs::read_dir(&first_path) else {
            continue;
        };
        for second in second_level.flatten() {
            let second_path = second.path();
            if !second_path.is_dir() {
                continue;
            }
            let nested = second_path.join(crate::cache_lib::prune_target::INCREMENTAL_SUBDIR);
            if nested.is_dir() {
                out.push(nested);
            }
        }
    }
}

fn augment_aborted_cargo_error(
    err: SoldrError,
    cleanup: CargoAbortCleanupReport,
    timeout: bool,
) -> SoldrError {
    let SoldrError::Other(mut message) = err else {
        return err;
    };
    message.push_str(&format!(
        "; soldr cleanup after abort: {}",
        cleanup.summary()
    ));
    if timeout {
        message.push_str(
            "; if the next build still stalls, run `soldr --no-cache cargo clean -p <crate>` \
             or remove the affected target/*/incremental directory, then retry the same command \
             as `soldr --no-cache cargo <same args>` or with `ZCCACHE_DISABLE=1`; use \
             `soldr logs paths` to inspect durable logs, and lower \
             `SOLDR_CARGO_WAIT_TIMEOUT_SECS` or `SOLDR_COMPILE_REPLY_TIMEOUT_SECS` while diagnosing",
        );
    }
    SoldrError::Other(message)
}

fn scrub_soldr_cache_lifecycle_env_for_child_cargo(command: &mut std::process::Command) {
    command.env_remove(SOLDR_CACHE_LIFECYCLE_ENV_VAR);
    command.env_remove(SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS_ENV_VAR);
}

fn scrub_inherited_soldr_workspace_env_for_child_cargo(command: &mut std::process::Command) {
    for key in INHERITED_SOLDR_WORKSPACE_ENV_VARS {
        command.env_remove(key);
    }
    for key in std::env::vars_os()
        .map(|(key, _)| key)
        .filter(|key| key.to_string_lossy().starts_with("SETUP_SOLDR_"))
    {
        command.env_remove(key);
    }
}

fn maybe_apply_rustfmt_zccache_shim(
    command: &mut std::process::Command,
    args: &[String],
    cache_enabled: bool,
) -> Option<crate::shim_dir::ShimDirGuard> {
    if !cache_enabled
        || !cargo_args_should_apply_rustfmt_shim(args)
        || std::env::var_os("RUSTFMT").is_some()
    {
        return None;
    }

    match crate::shim_dir::build_shim_dir() {
        Ok(guard) => {
            command.env(
                "RUSTFMT",
                crate::shim_dir::shim_tool_path(&guard.path, "rustfmt"),
            );
            command.env(crate::shim_dir::SOLDR_CHILD_SHIMS_ACTIVE_ENV_VAR, "1");
            Some(guard)
        }
        Err(err) => {
            eprintln!(
                "soldr warning: failed to build rustfmt shim for cargo fmt; rustfmt will run without zccache format caching: {err}"
            );
            None
        }
    }
}

pub(crate) async fn run_cargo_front_door(
    args: &[String],
    cache_enabled: bool,
    zccache_source: ZccacheSourceArg,
    trust_inherited_soldr_env: bool,
) -> Result<i32, SoldrError> {
    if cargo_args_use_reserved_no_cache(args) {
        return Err(SoldrError::Other(
            "`--no-cache` must appear before `cargo`, as in `soldr --no-cache cargo build`".into(),
        ));
    }

    // Parse the opt-in watchdog before starting daemons, spawning Cargo, or
    // mutating build-session state. Malformed configuration is a user-facing
    // error, not a reason to launch a child that would need cleanup.
    let cargo_wait_timeout = cargo_wait_timeout()?;

    let trust_inherited_soldr_env =
        trust_inherited_soldr_env || env_flag_truthy(crate::TRUST_INHERITED_SOLDR_ENV_VAR);
    let _fresh_workspace_env =
        FreshSoldrWorkspaceEnvGuard::apply_unless_trusted(trust_inherited_soldr_env);

    let cache_lifecycle = cache_lifecycle_from_env()?;
    let command_lifetime_shutdown_timeout = if cache_lifecycle == CacheLifecycle::Command {
        Some(command_lifetime_shutdown_timeout()?)
    } else {
        None
    };

    // soldr#1495: preflight the shared daemon once per managed build. If a
    // stale-version daemon is holding the endpoint (an older release still
    // serving compiles, or a protocol-mismatched daemon), displace it now
    // so this build's first rustc wrapper spawns a current-version daemon
    // instead of silently reusing stale embedded zccache or burning the
    // retry budget. Cheap when the daemon is already current or absent.
    if cache_enabled {
        if let Ok(paths) = crate::core::SoldrPaths::new() {
            crate::daemon::lifecycle::preflight_displace_stale_daemon(&paths);
        }
    }

    // Strip soldr-private auto target-GC opt-out flags before any other
    // arg-vector handling so downstream code (trampolines, cargo spawn)
    // never sees them. The env-var fallback is unioned in below.
    let (args_owned, gc_opt_out) = strip_no_gc_target_flags(args);
    let gc_opt_out = gc_opt_out.merged_with_env();
    let (args_owned, explicit_toolchain) = subcommand::strip_cargo_toolchain_directive(&args_owned);
    let explicit_toolchain = explicit_toolchain.as_deref();
    let args: &[String] = &args_owned;

    // `cargo run` trampoline (issue #344). When the binary is already
    // up-to-date with the recorded sources, this exec's the binary
    // directly and never spawns cargo. Otherwise we get back a plan that
    // strips the soldr-private `--no-trampoline` flag from the arg list
    // and lets us refresh the sidecar after cargo succeeds.
    let trampoline_plan = if subcommand::is_cargo_run_invocation(args) {
        match try_run_trampoline(args)? {
            TrampolineDecision::Executed(code) => return Ok(code),
            TrampolineDecision::FellThrough(plan) => Some(plan),
        }
    } else {
        None
    };

    // Workspace build/check/clippy freshness belongs to Cargo. The retired
    // sidecar path did not model Cargo's complete semantic identity and could
    // return false Fresh results (#1528). Keep accepting the historical
    // soldr-only opt-out flag as argument-cleanup compatibility, but always
    // invoke Cargo for these verbs.
    let workspace_args = matches!(
        first_cargo_subcommand(args),
        Some("build" | "b" | "check" | "c" | "clippy")
    )
    .then(|| strip_no_trampoline_flag(args).0);

    // Use the cleaned arg vector from here on so `--no-trampoline` is
    // not forwarded to cargo.
    let owned_cleaned_args;
    let args: &[String] = match (trampoline_plan.as_ref(), workspace_args.as_ref()) {
        (Some(plan), _) => {
            owned_cleaned_args = plan.cleaned_args.clone();
            &owned_cleaned_args
        }
        (None, Some(cleaned)) => {
            owned_cleaned_args = cleaned.clone();
            &owned_cleaned_args
        }
        (None, None) => args,
    };

    crate::toolchain::ensure_cargo_toolchain(explicit_toolchain)?;
    let cargo = resolve_toolchain_binary_for_channel("cargo", explicit_toolchain)?;
    let rustc = resolve_toolchain_binary_for_channel("rustc", explicit_toolchain)?;
    let cargo_bin_dir = cargo
        .parent()
        .ok_or_else(|| SoldrError::Other("failed to resolve cargo bin directory".into()))?
        .to_path_buf();
    let existing_path = std::env::var_os("PATH");
    let paths = SoldrPaths::new()?;
    paths.ensure_dirs()?;

    // L3 (soldr#980): kick off the managed zccache binary fetch +
    // extract + redb init on a background tokio task NOW. The rest of
    // this front-door pipeline — known-subcommand fetch, env scrub,
    // session-id stamp, target-registry memoization, pre-GC, low-disk
    // probe, profile_debug detection, linker injection — does not
    // depend on the resolved zccache path. Overlapping its wall-clock
    // cost with that synchronous setup is worth ~1-2 s on cold builds
    // where the binary is not already on disk. On warm builds the
    // background future resolves effectively immediately so the join
    // at `CargoCachePlan::finalize` is free.
    //
    // We intentionally spawn after the run-trampoline branch above because
    // that path exits without spawning cargo, and we don't
    // want to start a fetch we'll just drop. `cache_enabled` here is
    // the same flag the original synchronous `CargoCachePlan::prepare`
    // gated on; passing `false` produces a no-op `Disabled` prefetch.
    let cache_plan_prefetch =
        cache_plan::CargoCachePlanPrefetch::start(cache_enabled, &paths, zccache_source);

    // If the user invoked a known ecosystem subcommand (e.g. `cargo nextest`),
    // fetch the corresponding `cargo-<sub>` binary and prepend its directory to
    // PATH so cargo's subcommand dispatch finds it. Also collect transitive
    // bootstrap env (e.g. SDKROOT for explicit legacy
    // `cargo zigbuild --target *-apple-darwin`).
    let subcommand_tool_bootstrap = ensure_known_subcommand_tool(args, &paths).await?;
    let owned_bootstrap_args;
    let args: &[String] = if subcommand_tool_bootstrap.cargo_args.is_empty() {
        args
    } else {
        owned_bootstrap_args =
            insert_cargo_global_args(args, &subcommand_tool_bootstrap.cargo_args);
        &owned_bootstrap_args
    };
    let extra_bin_dirs = subcommand_tool_bootstrap.bin_dirs;
    let transitive_env_overrides = subcommand_tool_bootstrap.env;
    // Compute env-var overrides keyed off the subcommand + its
    // --target argument. Today this fixes ring's build.rs on
    // `cargo xwin build --target *-pc-windows-msvc` by routing cc-rs
    // to `clang-cl` instead of the GNU-flavoured `clang`. See
    // `compute_subcommand_env_overrides` for the full rule set.
    let subcommand_env_overrides = compute_subcommand_env_overrides(args);

    let mut command = std::process::Command::new(&cargo);
    command.args(args);
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    // These soldr control variables are consumed by this front-door
    // process. Letting cargo inherit them leaks daemon lifecycle policy
    // into build scripts and test binaries that may spawn nested soldr.
    scrub_soldr_cache_lifecycle_env_for_child_cargo(&mut command);
    if !trust_inherited_soldr_env {
        scrub_inherited_soldr_workspace_env_for_child_cargo(&mut command);
    }
    // soldr cargo is the top of the invocation tree, so any inherited
    // MAKEFLAGS/CARGO_MAKEFLAGS points at jobserver fds that aren't open in
    // our process. Stripping them lets cargo start a fresh jobserver instead
    // of printing the "failed to connect to jobserver" warning (see #283).
    command.env_remove("MAKEFLAGS");
    command.env_remove("CARGO_MAKEFLAGS");
    command.env("RUSTC", &rustc);

    // Issue #836 (sub of #835): pin the rust toolchain explicitly via
    // RUSTUP_TOOLCHAIN so rustup does NOT consult `rust-toolchain.toml`
    // on the cargo side and try to install the manifest's declared
    // `components = [...]` automatically.
    //
    // Why this matters in CI: many runner images (the GitHub-hosted
    // ubuntu-* lineage especially) ship a pre-existing `bin/cargo-fmt`
    // that conflicts with rustup's `rustfmt-preview` component install,
    // producing the well-known
    //
    //     error: failed to install component:
    //       'rustfmt-preview-x86_64-unknown-linux-gnu',
    //       detected conflict: 'bin/cargo-fmt'
    //
    // which kills the build before cargo even starts compiling. The
    // soldr bootstrap is supposed to short-circuit this — soldr itself
    // already knows the manifest channel (via
    // `read_rust_toolchain_manifest`), so passing it explicitly to
    // rustup with `RUSTUP_TOOLCHAIN` skips the manifest read on the
    // child cargo, and with it the entire auto-component-install path.
    //
    // Honor an explicit caller-set RUSTUP_TOOLCHAIN (don't clobber).
    // For users who genuinely need rustfmt / clippy at build time,
    // `soldr cargo fmt` / `clippy` still self-install via
    // `component_install::maybe_install_component_for_subcommand`.
    if let Some(toolchain) = explicit_toolchain {
        command.env("RUSTUP_TOOLCHAIN", toolchain);
    } else if std::env::var_os("RUSTUP_TOOLCHAIN").is_none() {
        let manifest_dir =
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        if let Ok(manifest) = crate::core::read_rust_toolchain_manifest(&manifest_dir) {
            if let Some(channel) = manifest.channel {
                let channel = channel.trim();
                if !channel.is_empty() {
                    command.env("RUSTUP_TOOLCHAIN", channel);
                }
            }
        }
    }

    // Apply subcommand-derived env overrides (e.g. CC_<triple>=clang-cl
    // for `cargo xwin build --target *-pc-windows-msvc`). Honor a
    // caller-set value — don't clobber if the user already exported
    // their own CC / CXX / AR.
    for (key, value) in &subcommand_env_overrides {
        if std::env::var_os(key).is_none() {
            command.env(key, value);
        }
    }
    // Apply transitive-bootstrap env overrides (e.g. SDKROOT for explicit
    // legacy `cargo zigbuild --target *-apple-darwin`). These come from
    // `ensure_known_subcommand_tool` which calls into ensure_apple_sdk
    // / ensure_zig / etc. The functions themselves already gate on
    // `var_os` being unset before pushing, so just apply them.
    for (key, value) in &transitive_env_overrides {
        command.env(key, value);
    }

    emit_zig_cross_linker_preflight(&command, args)?;

    let build_like_cargo = cargo_args_are_cacheable(args);
    // Issue #824 follow-up: always engage RUSTC_WRAPPER + the zccache
    // session when caching is enabled, regardless of whether the cargo
    // subcommand is in our known-compiling set. The previous policy
    // (`cache_enabled && build_like_cargo`) silently dropped rustc
    // observations whenever soldr's classifier said "this subcommand
    // doesn't compile" — but build scripts, third-party cargo subcommand
    // plugins not yet in `known_tools`, and even some normally-non-
    // compiling verbs *can* re-shell to rustc through paths we don't
    // model. We always want zccache to see the call, then have zccache
    // itself decide whether to cache or pass through (its "non-cacheable"
    // classifier already handles read-only / non-hashable inputs).
    //
    // The trade-off is a small session-start/stop overhead (~hundreds of
    // ms) for cargo subcommands that don't end up spawning rustc — but
    // the observability win is "no rustc call goes unrecorded". The other
    // hooks (cook hydrate, disk watchdog, target-registry memo) still
    // gate on `build_like_cargo` because those have nothing to do with
    // rustc wrapping — they care about whether `target/` will be touched.
    let cache_enabled_for_cargo = cache_enabled;

    // Issue #597: auto-install rustup components for `soldr cargo {fmt,
    // clippy,miri}` when they're missing. Best-effort and silent on
    // failure — cargo's own error surfaces if the auto-install fails.
    // Honors SOLDR_NO_AUTO_COMPONENT=1.
    component_install::maybe_install_component_for_subcommand(args, &paths);

    // PR 3 (#578, meta #579): cross-repo cook-index pre-flight hydrate.
    // Best-effort — every failure path is silent so a missing daemon,
    // missing Cargo.lock, mismatched sha, or extract error never
    // breaks the cargo build. Only fires for build-like cargo
    // commands; `cargo metadata` / `cargo search` / etc. don't need
    // target/ to be populated.
    if build_like_cargo {
        cook_hydrate::maybe_hydrate(args, &paths, &rustc);
    }

    let cargo_profile_debug_default = if build_like_cargo {
        profile_debug::maybe_apply_cargo_profile_debug_default(&mut command, args, &paths)?
    } else {
        None
    };

    let cargo_subcommand = first_cargo_subcommand(args);
    let pyo3_build = matches!(
        cargo_subcommand,
        Some(
            "b" | "build"
                | "c"
                | "check"
                | "t"
                | "test"
                | "bench"
                | "d"
                | "doc"
                | "r"
                | "run"
                | "clippy"
                | "fix"
        )
    ) || cargo_subcommand == Some(concat!("rust", "c"));
    if build_like_cargo {
        // Cargo front door only: keep startup/low-disk warnings off unrelated
        // commands and out of the rustc-wrapper hot path.
        gc::emit_startup_target_warning_if_due();
    }
    let mut path_dirs: Vec<std::path::PathBuf> = Vec::with_capacity(1 + extra_bin_dirs.len());
    path_dirs.push(cargo_bin_dir);
    path_dirs.extend(extra_bin_dirs);
    command.env(
        "PATH",
        disk::prepend_paths(&path_dirs, existing_path.as_deref())?,
    );
    let _rustfmt_shim_guard =
        maybe_apply_rustfmt_zccache_shim(&mut command, args, cache_enabled_for_cargo);
    let explicit_target = target::default_cargo_build_target(args)?;
    if let Some(target) = explicit_target.as_deref() {
        command.env("CARGO_BUILD_TARGET", target);
    }
    let known_cargo_target = target::known_cargo_build_target(args, explicit_target.as_deref());
    // soldr#1610/#1614: every cargo-backed build surface consumes the
    // same target-aware PyO3 plan. The resolver is conservative: it only
    // injects PYO3_NO_PYTHON for a proven cross ABI3 extension, never for
    // embedding/non-ABI3 builds, and never downloads Python assets merely
    // because PyO3 appears in metadata.
    if pyo3_build {
        let workspace_root =
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let mut pyo3_plan = crate::pyo3_detect::resolve_for_cargo_invocation(
            &workspace_root,
            args,
            known_cargo_target.as_deref(),
        );
        pyo3_plan.materialize_compatibility(&paths).await?;
        pyo3_plan.emit_diagnostic();
        pyo3_plan.apply_to_command(&mut command);
    }
    let native_cache_target = known_cargo_target.filter(|target| target.ends_with("-apple-darwin"));

    target::apply_linker_override(&mut command, args, explicit_target.as_deref(), &paths)?;

    // L3 (soldr#980): await the background zccache prefetch we kicked
    // off near the top of this function. Up until this point the cargo
    // command has been built without any wrapper env, so the prefetch
    // has been overlapping the entire setup pipeline. On a cold build
    // (binary not yet on disk) this is where the ~1-2 s saving falls
    // out — on a warm build the await is a near-no-op.
    //
    // Note: `cache_enabled_for_cargo` is currently `cache_enabled` (see
    // the comment above its assignment for the #824 follow-up
    // rationale). We thread it through `finalize` for symmetry with the
    // old synchronous API so that future divergence between the two
    // flags doesn't silently rewire the prefetch decision.
    let mut cache_plan =
        CargoCachePlan::finalize(cache_enabled_for_cargo, cache_plan_prefetch).await?;
    cache_plan.apply_to_command(&mut command, native_cache_target.as_deref())?;

    cache_plan.prepare_rust_artifact_plan(
        &cargo,
        &rustc,
        args,
        cargo_profile_debug_default.as_ref(),
    )?;
    let capture_cargo_artifacts = build_like_cargo
        && cache_plan.has_rust_artifact_plan()
        && !cargo_args_have_message_format(args);
    if capture_cargo_artifacts {
        // Cargo's JSON stream is line-oriented and preserves rendered
        // diagnostics in the message payload. It lets us build an exact
        // package-aware closure while teeing the bytes unchanged below.
        command.arg("--message-format=json");
    }
    if build_like_cargo {
        let probe_path = cache_plan
            .target_dir_for_hooks(args)
            .unwrap_or_else(|| disk::cargo_disk_space_probe_path(args));
        disk::maybe_emit_low_disk_warning(&probe_path);
        // Issue #574: host-volume disk watchdog. Distinct from the
        // legacy 2 GiB advisory above — this layer warns at 10 GiB and
        // aborts at 5 GiB so cross-repo target/ bloat surfaces before
        // the build sets the disk on fire. Returning Err here lets the
        // top-level dispatch print the error and exit with a non-zero
        // code (same path as any other SoldrError from the front door).
        let watchdog_path = cache_plan
            .target_dir_for_hooks(args)
            .unwrap_or_else(|| disk::cargo_disk_space_probe_path(args));
        match gc::disk::check_disk_or_warn_or_block(&watchdog_path) {
            gc::disk::DiskCheckOutcome::Disabled | gc::disk::DiskCheckOutcome::Ok { .. } => {}
            gc::disk::DiskCheckOutcome::Warn {
                free_bytes,
                threshold_gib,
            } => {
                eprintln!("{}", gc::disk::render_warn_line(free_bytes, threshold_gib));
            }
            gc::disk::DiskCheckOutcome::Block {
                free_bytes,
                threshold_gib,
            } => {
                return Err(SoldrError::Other(gc::disk::render_block_message(
                    free_bytes,
                    threshold_gib,
                )));
            }
        }
    }
    let restore_outcome = cache_plan.restore_rust_artifacts()?;

    // A preceding cached build may have materialized immutable outputs as
    // protected hardlinks to cache blobs. Whenever the finalized wrapper plan
    // has no managed zccache session, detach shared target files locally
    // before the unmediated compiler can overwrite them. This must not depend
    // on the daemon being responsive. Conservatively include `install`:
    // configuration can select a persistent target root without a visible
    // command-line or environment override.
    if cargo_args_may_compile_unmediated(args) && cache_plan.zccache_session().is_none() {
        let report = no_cache_detach::prepare_target_for_unmediated_build(&cargo, args, &command)?;
        if report.detached_shared > 0 || report.made_writable > 0 {
            eprintln!(
                "soldr: no-cache preflight prepared {}: detached {} shared file(s), made {} private file(s) writable",
                report.target_dir.display(),
                report.detached_shared,
                report.made_writable,
            );
        }
    }

    // Target-registry memoization for the wrapper hot path (#440).
    // Without this, every rustc invocation re-opens redb and writes
    // the same target row (~14 ms p50 on Windows in the issue #440
    // profile). The cargo front door runs once per build session and
    // already knows the target dir, so do the upsert here and
    // propagate a recorded-marker env var that lets the wrapper skip
    // its own redb work + daemon target-touch IPC.
    if build_like_cargo {
        let target_dir_for_memo: Option<std::path::PathBuf> = cache_plan.target_dir_for_hooks(args);
        if let Some(dir) = target_dir_for_memo.as_deref() {
            apply_target_registry_memo(&mut command, dir, &paths);
        }
    }

    // Pre-compile target-GC (#485). Only on build-like cargo invocations
    // (build/check/test/run/...) and only when the user hasn't opted out
    // via --no-gc-target / --no-gc-target-before / SOLDR_NO_GC_TARGET.
    // Uses the rust_plan target_dir when available so the hook respects
    // any CARGO_TARGET_DIR / --target-dir override the same way cargo
    // and rust_plan do.
    //
    // Restore-aware since issue #1558: when the verified rust-plan
    // restore above just materialized files into a fresh target/, the
    // destructive keep-latest pass is skipped so Cargo — the freshness
    // authority — evaluates the restored hash families first. See
    // `run_pre_cargo_target_gc`.
    if build_like_cargo && !gc_opt_out.before {
        let target_dir = cache_plan.target_dir_for_hooks(args);
        if let Some(dir) = target_dir.as_deref() {
            if let Some(outcome) = run_pre_cargo_target_gc(dir, &restore_outcome) {
                emit_auto_prune_summary(&outcome);
            }
        }
    }

    // Capture when stderr is not a terminal (CI / Docker
    // / `soldr cargo build 2>file`) so the cargo_diagnostics scanner can
    // recognize the missing-host-tool failure pattern from #422 and
    // rewrap cargo's terse `failed to execute command: (os error 2)`
    // with platform-aware install hints. Interactive TTY users keep
    // `.status()` inheritance (and therefore cargo's live progress bar)
    // since changing stderr to a pipe would force cargo into its
    // non-TTY rendering mode.
    use std::io::IsTerminal;
    let capture_for_diagnostics = !std::io::stderr().is_terminal();

    // Phase 2: start session correlation only after every fallible pre-cargo
    // preparation step (especially no-cache ownership detachment) succeeds.
    // From here, the cargo runner's success/error paths always pair this with
    // BuildSessionEnd and clear build_active, so a rejected preflight cannot
    // strand daemon maintenance in the "build active" state.
    let session_id = generate_build_session_id();
    command.env(
        crate::cache_lib::SOLDR_BUILD_SESSION_ID_ENV_VAR,
        session_id.to_string(),
    );
    let session_started_at_ms = current_unix_ms();
    let session_repo_root =
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    if crate::daemon::client::build_session_start(
        &paths,
        session_id,
        &session_repo_root,
        session_started_at_ms,
    )
    .is_err()
    {
        persist_build_session_start_fallback(
            &paths,
            session_id,
            &session_repo_root,
            session_started_at_ms,
        );
    }
    let build_activity_lease =
        crate::cache_lib::build_active::BuildActivityLease::acquire(&paths, session_id).map_err(
            |error| SoldrError::Other(format!("failed to acquire build activity lease: {error}")),
        )?;
    // Issue #980 L7: gate long-running workers only for the interval where
    // child Cargo may actually be active.
    crate::cache_lib::build_active::set(true);

    // soldr#1368 observability restore: snapshot the embedded zccache
    // compile counters just before cargo runs so `finish_zccache_session`
    // can diff start-vs-end into the per-build hit/miss summary written to
    // `last-session-stats.json`.
    if let Some(session) = cache_plan.zccache_session() {
        crate::cache::capture_build_baseline(&session.cache_dir, &session.session_id);
    }
    let compile_journal_start_len = file_len(&embedded_compile_journal_path(&paths));
    let cargo_run_result: CargoRunResult = if capture_cargo_artifacts {
        let target_dir = cache_plan
            .target_dir_for_hooks(args)
            .unwrap_or_else(|| disk::cargo_disk_space_probe_path(args));
        run_command_capturing_cargo_json(&mut command, &target_dir, cargo_wait_timeout)
            .map(|(status, captured, paths)| (status, Some(captured), Some(paths)))
    } else if capture_for_diagnostics {
        run_command_capturing_diagnostic_tail(&mut command, cargo_wait_timeout)
            .map(|(status, captured)| (status, Some(captured), None))
    } else {
        run_command_inheriting_stdio(&mut command, cargo_wait_timeout)
            .map(|status| (status, None, None))
    };
    let (status, diagnostic_capture, cargo_artifact_paths) = match cargo_run_result {
        Ok(outcome) => outcome,
        Err(err) => {
            let timeout = cargo_run_error_is_timeout(&err);
            let ended_at_ms = current_unix_ms();
            let daemon_finalized =
                crate::daemon::client::build_session_end(&paths, session_id, -1, ended_at_ms)
                    .is_ok();
            if !daemon_finalized {
                persist_build_session_end_fallback(&paths, session_id, -1, ended_at_ms);
            }
            crate::cache_lib::build_active::set(false);
            drop(build_activity_lease);
            let cleanup = cleanup_after_aborted_cargo_run(&cache_plan, args, timeout);
            let finish_result =
                cache_plan.finish_zccache_session(command_lifetime_shutdown_timeout);
            if let Some(session) = cache_plan.zccache_session() {
                persist_build_log_history(BuildLogHistoryRequest {
                    paths: &paths,
                    build_session_id: session_id,
                    repo_root: &session_repo_root,
                    started_at_ms: session_started_at_ms,
                    session,
                    compile_journal_start_len,
                    exit_code: -1,
                    ended_at_ms,
                    daemon_finalized,
                });
            }
            if let Err(finish_err) = finish_result {
                eprintln!(
                    "soldr warning: failed to finish zccache session after aborted cargo run: {finish_err}"
                );
            }
            let augmented = augment_aborted_cargo_error(err, cleanup, timeout);
            let auto_retry_planned =
                timeout && cargo_timeout_retry_allowed(cache_enabled_for_cargo, args);
            match append_cargo_abort_log(CargoAbortLogRequest {
                paths: &paths,
                session_id,
                repo_root: &session_repo_root,
                started_at_ms: session_started_at_ms,
                ended_at_ms,
                args,
                timeout,
                cargo_wait_timeout,
                cleanup,
                message: &augmented.to_string(),
                auto_retry_planned,
            }) {
                Ok(path) => eprintln!("soldr: cargo abort details written to {}", path.display()),
                Err(log_err) => {
                    eprintln!("soldr warning: failed to write cargo abort log: {log_err}")
                }
            }
            if auto_retry_planned {
                eprintln!(
                    "soldr: retrying timed-out cargo run without cache: soldr --no-cache cargo <same args>"
                );
                match retry_timed_out_cargo_without_cache(args, explicit_toolchain) {
                    Ok(status) => {
                        let code = status
                            .code()
                            .unwrap_or(if status.success() { 0 } else { 1 });
                        eprintln!("soldr: no-cache cargo retry exited with code {code}");
                        return Ok(code);
                    }
                    Err(retry_err) => {
                        return Err(SoldrError::Other(format!(
                            "{augmented}; no-cache retry failed: {retry_err}"
                        )));
                    }
                }
            }
            return Err(augmented);
        }
    };
    let captured_stderr_for_diagnosis = diagnostic_capture;

    // Phase 2: send BuildSessionEnd before the success/failure
    // branches do any further work. Best-effort — never affects the
    // build's own outcome. soldr#1536: the daemon acknowledges once the
    // finalized aggregate and every session event are durable; on any
    // error we fall back to the direct-redb finalization below.
    let ended_at_ms = current_unix_ms();
    let daemon_finalized = crate::daemon::client::build_session_end(
        &paths,
        session_id,
        status.code().unwrap_or(-1),
        ended_at_ms,
    )
    .is_ok();
    if !daemon_finalized {
        persist_build_session_end_fallback(
            &paths,
            session_id,
            status.code().unwrap_or(-1),
            ended_at_ms,
        );
    }
    // Issue #980 L7: paired with the `set(true)` above. Clearing here
    // (before `post_cargo_result`) lets the post-build target-GC pass
    // run normally without thinking it's still inside the build.
    crate::cache_lib::build_active::set(false);
    drop(build_activity_lease);
    // Issue #1286 (F5): the build just ended — this is the idle
    // transition, so fire the auto-GC sweep now, as a detached process
    // that survives this wrapper's imminent exit. Throttled to once
    // per 5-minute window by the marker inside.
    if build_like_cargo {
        gc::maybe_spawn_auto_gc_sweeper(&paths);
    }

    let post_cargo_result: Result<(), SoldrError> = (|| {
        if status.success() {
            if let Some(paths) = cargo_artifact_paths.as_deref() {
                cache_plan.record_cargo_artifact_closure(paths, !paths.is_empty())?;
            }
            cache_plan.save_rust_artifacts(restore_outcome)?;
            // Post-compile target-GC (#485). Same gating as the pre-pass —
            // build-like cargo, no opt-out, resolve dir consistently with the
            // pre-pass. The active-cargo-lock guard inside `auto_prune_target`
            // is what keeps a parallel `cargo` in the same `target/` from
            // racing this pass; we never emit a stderr line when that guard
            // engages.
            if build_like_cargo && !gc_opt_out.after {
                let target_dir = cache_plan.target_dir_for_hooks(args);
                if let Some(dir) = target_dir.as_deref() {
                    let outcome = auto_prune_target(dir, AutoPrunePhase::After);
                    emit_auto_prune_summary(&outcome);
                }
            }
            if let Some(plan) = trampoline_plan.as_ref() {
                refresh_sidecar_after_cargo(plan);
            }
        } else {
            // A non-zero cargo exit can leave orphan `.rmeta` files (rmeta
            // emitted, then rustc aborted before the `.rlib` codegen pass)
            // in `target/<triple>/<profile>/deps/`. Subsequent invocations
            // then fail with `E0463: can't find crate` because cargo passes
            // `--extern X=orphan.rmeta` to dependents and rustc cannot link
            // an rmeta-only crate. Sweep them so the next build rebuilds
            // cleanly. See soldr#410.
            cache_plan.prune_orphan_rmetas_after_failed_build();
        }
        Ok(())
    })();

    // After cargo fails, look at whatever stderr we captured for a
    // recognizable build-script-spawn-ENOENT pattern (#422 — minimal
    // Rust containers without a host C toolchain). The capture
    // source is the diagnostic-tail buffer. TTY users captured
    // nothing — they see cargo's own error untouched and skip this
    // path.
    if !status.success() {
        if let Some(stderr_text) = captured_stderr_for_diagnosis.as_deref() {
            if let Some(diag) = crate::cargo_diagnostics::detect_build_script_failure(stderr_text) {
                let rendered = crate::cargo_diagnostics::render_diagnosis(&diag);
                let stderr = std::io::stderr();
                let _ = stderr.lock().write_all(rendered.as_bytes());
            }
        }
    }

    let finish_result = cache_plan.finish_zccache_session(command_lifetime_shutdown_timeout);
    if let Some(session) = cache_plan.zccache_session() {
        persist_build_log_history(BuildLogHistoryRequest {
            paths: &paths,
            build_session_id: session_id,
            repo_root: &session_repo_root,
            started_at_ms: session_started_at_ms,
            session,
            compile_journal_start_len,
            exit_code: status.code().unwrap_or(-1),
            ended_at_ms,
            daemon_finalized,
        });
    }
    finish_result?;
    post_cargo_result?;
    drop(trampoline_plan);
    Ok(status.code().unwrap_or(1))
}

fn cargo_args_have_message_format(args: &[String]) -> bool {
    args.iter()
        .any(|arg| arg == "--message-format" || arg.starts_with("--message-format="))
}

fn run_command_capturing_cargo_json(
    command: &mut std::process::Command,
    target_dir: &Path,
    timeout: Option<Duration>,
) -> Result<(std::process::ExitStatus, String, Vec<String>), SoldrError> {
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    configure_cargo_child_for_timeout(command);
    let mut child = command
        .spawn()
        .map_err(|err| SoldrError::Other(format!("spawn cargo for JSON capture failed: {err}")))?;
    let stdout_rx = spawn_capture_pipe_reader_to_stdout(child.stdout.take().expect("piped"));
    let stderr_rx = spawn_capture_pipe_reader(child.stderr.take().expect("piped"));
    let status = wait_for_cargo_child(&mut child, "cargo JSON capture", timeout)?;
    let stdout = drain_capture_pipe_after_child_exit(&stdout_rx, "cargo JSON stdout");
    let stderr = drain_capture_pipe_after_child_exit(&stderr_rx, "cargo JSON stderr");
    let paths = parse_cargo_artifact_closure(&stdout, target_dir);
    Ok((status, String::from_utf8_lossy(&stderr).into_owned(), paths))
}

fn parse_cargo_artifact_closure(stdout: &[u8], target_dir: &Path) -> Vec<String> {
    let mut paths = BTreeMap::<String, ()>::new();
    let mut complete = true;
    for line in String::from_utf8_lossy(stdout).lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            complete = false;
            continue;
        };
        let Some(reason) = value.get("reason").and_then(serde_json::Value::as_str) else {
            continue;
        };
        match reason {
            "compiler-artifact" => {
                if let Some(filenames) =
                    value.get("filenames").and_then(serde_json::Value::as_array)
                {
                    for filename in filenames.iter().filter_map(serde_json::Value::as_str) {
                        add_cargo_closure_path(&mut paths, Path::new(filename), target_dir);
                    }
                } else {
                    complete = false;
                }
            }
            "build-script-executed" => {
                if let Some(out_dir) = value.get("out_dir").and_then(serde_json::Value::as_str) {
                    add_cargo_closure_path(&mut paths, Path::new(out_dir), target_dir);
                } else {
                    complete = false;
                }
            }
            "compiler-message" | "build-finished" | "text" => {}
            _ => complete = false,
        }
    }
    if !complete || paths.is_empty() {
        return Vec::new();
    }
    paths.into_keys().collect()
}

fn add_cargo_closure_path(paths: &mut BTreeMap<String, ()>, path: &Path, target_dir: &Path) {
    let Ok(relative) = path.strip_prefix(target_dir) else {
        return;
    };
    if !relative.as_os_str().is_empty() {
        paths.insert(relative.to_string_lossy().replace('\\', "/"), ());
    }
    if path
        .components()
        .any(|component| component.as_os_str() == ".fingerprint")
    {
        return;
    }
    if let Some(parent) = path.parent() {
        if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
            let fingerprint_name = stem.strip_prefix("lib").unwrap_or(stem);
            let fingerprint_dir = parent
                .parent()
                .map(|profile| profile.join(".fingerprint").join(fingerprint_name));
            if let Some(dir) = fingerprint_dir {
                collect_closure_files(paths, &dir, target_dir);
            }
        }
    }
    if path.is_dir() {
        collect_closure_files(paths, path, target_dir);
    }
}

fn collect_closure_files(paths: &mut BTreeMap<String, ()>, root: &Path, target_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_closure_files(paths, &path, target_dir);
        } else if path.is_file() {
            add_cargo_closure_path(paths, &path, target_dir);
        }
    }
}

fn spawn_capture_pipe_reader_to_stdout<R>(
    mut reader: R,
) -> std::sync::mpsc::Receiver<CapturePipeMessage>
where
    R: std::io::Read + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    let bytes = chunk[..n].to_vec();
                    let _ = std::io::stdout().lock().write_all(&bytes);
                    let _ = tx.send(CapturePipeMessage::Chunk(bytes));
                }
                Err(_) => break,
            }
        }
        let _ = std::io::stdout().lock().flush();
        let _ = tx.send(CapturePipeMessage::Eof);
    });
    rx
}

fn run_command_inheriting_stdio(
    command: &mut std::process::Command,
    timeout: Option<Duration>,
) -> Result<std::process::ExitStatus, SoldrError> {
    configure_cargo_child_for_timeout(command);
    let mut child = command
        .spawn()
        .map_err(|err| SoldrError::Other(format!("spawn cargo failed: {err}")))?;
    wait_for_cargo_child(&mut child, "cargo", timeout)
}

/// Run cargo with both streams tee'd to the user's stdout/stderr AND
/// stderr accumulated into a [`String`] for post-failure scanning by
/// [`crate::cargo_diagnostics`]. Stdout is NOT buffered — we only need
/// stderr for diagnosis, and cargo can emit megabytes of compile
/// progress to stdout that would just sit unused in RAM.
///
/// Used in the non-clippy, non-TTY branch of `run_cargo_front_door`
/// (#422): when stderr is piped to a CI log / Docker stream / file,
/// cargo's progress-bar UX is already gone, so the extra
/// pipe-and-tee doesn't degrade interactive output.
fn run_command_capturing_diagnostic_tail(
    command: &mut std::process::Command,
    timeout: Option<Duration>,
) -> Result<(std::process::ExitStatus, String), SoldrError> {
    command.stderr(std::process::Stdio::piped());
    // stdout stays inherited — we don't need its bytes.
    configure_cargo_child_for_timeout(command);
    let mut child = command.spawn().map_err(|err| {
        SoldrError::Other(format!("spawn cargo for diagnostic capture failed: {err}"))
    })?;
    let child_stderr = child.stderr.take().expect("piped");

    let stderr_rx = spawn_capture_pipe_reader(child_stderr);

    let status = wait_for_cargo_child(&mut child, "cargo diagnostic capture", timeout)?;
    let bytes = drain_capture_pipe_after_child_exit(&stderr_rx, "cargo diagnostic stderr");
    let captured = String::from_utf8_lossy(&bytes).into_owned();
    Ok((status, captured))
}

enum CapturePipeMessage {
    Chunk(Vec<u8>),
    Eof,
}

fn spawn_capture_pipe_reader<R>(mut reader: R) -> std::sync::mpsc::Receiver<CapturePipeMessage>
where
    R: std::io::Read + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    let bytes = chunk[..n].to_vec();
                    let stderr = std::io::stderr();
                    let _ = stderr.lock().write_all(&bytes);
                    let _ = tx.send(CapturePipeMessage::Chunk(bytes));
                }
                Err(_) => break,
            }
        }
        let stderr = std::io::stderr();
        let _ = stderr.lock().flush();
        let _ = tx.send(CapturePipeMessage::Eof);
    });
    rx
}

fn drain_capture_pipe_after_child_exit(
    rx: &std::sync::mpsc::Receiver<CapturePipeMessage>,
    context: &str,
) -> Vec<u8> {
    let deadline = Instant::now() + CAPTURE_PIPE_EOF_GRACE;
    let mut buf = Vec::new();
    loop {
        match rx.try_recv() {
            Ok(CapturePipeMessage::Chunk(bytes)) => {
                buf.extend_from_slice(&bytes);
                continue;
            }
            Ok(CapturePipeMessage::Eof) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                return buf;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }

        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            eprintln!(
                "soldr: {context} pipe did not close within {}ms after cargo exited; \
                 continuing with captured output",
                CAPTURE_PIPE_EOF_GRACE.as_millis()
            );
            return buf;
        };
        match rx.recv_timeout(remaining) {
            Ok(CapturePipeMessage::Chunk(bytes)) => buf.extend_from_slice(&bytes),
            Ok(CapturePipeMessage::Eof) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return buf;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                eprintln!(
                    "soldr: {context} pipe did not close within {}ms after cargo exited; \
                     continuing with captured output",
                    CAPTURE_PIPE_EOF_GRACE.as_millis()
                );
                return buf;
            }
        }
    }
}

/// Decide whether the "did you mean: cargo X?" hint applies to a typed
/// subcommand that isn't in `known_tools`. Returns `Some(suggestion)`
/// only when `sub` looks like a typo of a registered cargo subcommand
/// AND is not itself a legitimate cargo built-in verb.
///
/// Issue #755: without the built-in guard, `soldr cargo check` falsely
/// suggested `cargo chef` (Levenshtein distance 2). Built-in verbs are
/// routed through the External arm by `cargo` itself; treating them as
/// typos contradicts the contract documented in `CARGO_BUILTIN_VERBS`.
fn suggest_cargo_subcommand_typo(sub: &str) -> Option<String> {
    if crate::cli_args::is_cargo_builtin_verb(sub) {
        return None;
    }
    let known = crate::fetch::known_cargo_subcommands();
    crate::fuzzy_match::suggest_close_match(sub, &known).map(|s| s.to_string())
}

/// Env var name for the PATH-first override (issue #816). Reads as truthy
/// when set to any value except an empty string or `0`/`false`/`no`.
pub(crate) const FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR: &str =
    "SOLDR_FORCE_MANAGED_CARGO_SUBCOMMANDS";

fn force_managed_cargo_subcommands() -> bool {
    match std::env::var(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR) {
        Ok(value) => {
            let trimmed = value.trim();
            !matches!(trimmed, "" | "0" | "false" | "no" | "off")
        }
        Err(_) => false,
    }
}

/// Walk `$PATH` looking for an executable named `tool`. Mirrors the
/// hand-rolled lookup in `core::toolchain_resolve::path_bin_dir` —
/// duplicated rather than re-exported to keep the cargo-front-door
/// independent of `core::toolchain_resolve`'s internals. On Windows the
/// `PATHEXT` suffix sweep matches what the toolchain resolver does so
/// `cargo-zigbuild.exe` is found even when the caller typed `cargo-zigbuild`.
fn find_on_path(tool: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(tool);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            if std::path::Path::new(tool).extension().is_some() {
                continue;
            }
            let pathext = std::env::var_os("PATHEXT")
                .and_then(|value| value.into_string().ok())
                .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_string());
            for suffix in pathext.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                let suffixed = dir.join(format!("{tool}{suffix}"));
                if suffixed.is_file() {
                    return Some(suffixed);
                }
            }
        }
    }
    None
}

/// Result of subcommand tool resolution: PATH-prepended bin dirs +
/// env-var overrides for the child cargo invocation.
pub(crate) struct SubcommandToolBootstrap {
    pub bin_dirs: Vec<std::path::PathBuf>,
    pub env: Vec<(String, String)>,
    pub cargo_args: Vec<String>,
}

async fn ensure_known_subcommand_tool(
    args: &[String],
    paths: &SoldrPaths,
) -> Result<SubcommandToolBootstrap, SoldrError> {
    let Some(sub) = first_cargo_subcommand(args) else {
        return Ok(SubcommandToolBootstrap {
            bin_dirs: Vec::new(),
            env: Vec::new(),
            cargo_args: Vec::new(),
        });
    };
    let Some(spec) = crate::fetch::lookup_by_cargo_subcommand(sub) else {
        // Issue #412: when the typed subcommand isn't in
        // `known_tools` but LOOKS like a typo of one that IS, drop a
        // "did you mean?" hint on stderr. We still return empty so the
        // underlying cargo invocation continues as today — the
        // suggestion is advisory and cargo's own external-command
        // dispatch may still find the tool on PATH.
        if let Some(suggestion) = suggest_cargo_subcommand_typo(sub) {
            eprintln!("soldr: '{sub}' is not a cargo subcommand soldr ships a prebuilt for.");
            eprintln!("soldr: did you mean: cargo {suggestion}?");
        }
        return Ok(SubcommandToolBootstrap {
            bin_dirs: Vec::new(),
            env: Vec::new(),
            cargo_args: Vec::new(),
        });
    };

    // Issue #816: if `cargo-<sub>` is already on PATH, defer to it instead
    // of running the managed fetch. This matches the discipline
    // `ensure_rustup_available` uses for rustup and avoids two failure modes:
    //   1. The managed fetcher writing an unrunnable artifact (the original
    //      #816 / #810 cargo-zigbuild bug, now fixed by xz2 extraction —
    //      but PATH-first is a structural belt-and-suspenders).
    //   2. Bypassing a user who deliberately installed a specific version
    //      via `cargo install <name> --locked` or their distro package
    //      manager. cargo's own external-subcommand dispatch will find the
    //      PATH binary; soldr returning Ok(empty) here leaves that path
    //      open without prepending its own bin dir.
    // Escape hatch: SOLDR_FORCE_MANAGED_CARGO_SUBCOMMANDS=1 forces the
    // managed fetch even when PATH has the tool — useful for CI runs that
    // want byte-identical pinned binaries.
    let mut extra_bin_dirs: Vec<std::path::PathBuf> = Vec::new();
    let mut extra_env: Vec<(String, String)> = Vec::new();
    let mut extra_cargo_args: Vec<String> = Vec::new();

    if !force_managed_cargo_subcommands() {
        let exe_name = format!("cargo-{sub}");
        if let Some(path) = find_on_path(&exe_name) {
            eprintln!(
                "soldr: deferring to {exe_name} on PATH at {} (set SOLDR_FORCE_MANAGED_CARGO_SUBCOMMANDS=1 to override)",
                path.display()
            );
            // Even when cargo-zigbuild is provided by the host, it
            // still shells out to `zig`. Run the transitive bootstrap
            // before returning so the deferred-on-PATH branch doesn't
            // silently regress.
            append_subcommand_transitive_bin_dirs(
                sub,
                args,
                paths,
                &mut extra_bin_dirs,
                &mut extra_env,
                &mut extra_cargo_args,
            )
            .await?;
            return Ok(SubcommandToolBootstrap {
                bin_dirs: extra_bin_dirs,
                env: extra_env,
                cargo_args: extra_cargo_args,
            });
        }
    }

    // cargo-dylint v6.0.1 publishes Linux GNU release assets, but not
    // Windows or macOS ones. Keep its normal managed-fetch path on the
    // supported host and use Soldr's pinned, wrapper-free source-build
    // path elsewhere. The result is cached below ~/.soldr/bin, just like
    // the explicitly requested soldr build-from-source flow.
    if sub == "dylint" && dylint_requires_source_build() {
        let plan = crate::build_from_source_cmd::resolve_plan("cargo-dylint", None, None, paths)?;
        let binary = if plan.final_binary.is_file() {
            eprintln!(
                "soldr: using cached source-built cargo-dylint at {}",
                plan.final_binary.display()
            );
            plan.final_binary.clone()
        } else {
            eprintln!(
                "soldr: cargo-dylint has no prebuilt asset for this host; building pinned source fallback..."
            );
            crate::build_from_source_cmd::execute_plan(&plan)?.binary
        };
        let dir = binary.parent().ok_or_else(|| {
            SoldrError::Other(format!(
                "failed to resolve bin dir for source-built cargo-dylint: {}",
                binary.display()
            ))
        })?;
        extra_bin_dirs.push(dir.to_path_buf());
        append_subcommand_transitive_bin_dirs(
            sub,
            args,
            paths,
            &mut extra_bin_dirs,
            &mut extra_env,
            &mut extra_cargo_args,
        )
        .await?;
        return Ok(SubcommandToolBootstrap {
            bin_dirs: extra_bin_dirs,
            env: extra_env,
            cargo_args: extra_cargo_args,
        });
    }
    let version = spec
        .pinned_version
        .map(|v| VersionSpec::Exact(v.to_string()))
        .unwrap_or(VersionSpec::Latest);

    eprintln!("soldr: fetching {}...", spec.crate_name);
    let result =
        crate::fetch::fetch_tool_for_host_with_paths(spec.crate_name, &version, paths).await?;

    if result.cached {
        eprintln!(
            "soldr: using cached {} v{}",
            spec.crate_name, result.version
        );
    } else {
        eprintln!("soldr: downloaded {} v{}", spec.crate_name, result.version);
    }

    let dir = result
        .binary_path
        .parent()
        .ok_or_else(|| {
            SoldrError::Other(format!(
                "failed to resolve bin dir for fetched {}",
                spec.crate_name
            ))
        })?
        .to_path_buf();
    extra_bin_dirs.push(dir);
    append_subcommand_transitive_bin_dirs(
        sub,
        args,
        paths,
        &mut extra_bin_dirs,
        &mut extra_env,
        &mut extra_cargo_args,
    )
    .await?;
    Ok(SubcommandToolBootstrap {
        bin_dirs: extra_bin_dirs,
        env: extra_env,
        cargo_args: extra_cargo_args,
    })
}

fn dylint_requires_source_build() -> bool {
    !cfg!(all(target_os = "linux", target_env = "gnu"))
}

fn insert_cargo_global_args(args: &[String], cargo_args: &[String]) -> Vec<String> {
    if cargo_args.is_empty() {
        return args.to_vec();
    }
    let mut out = args.to_vec();
    let insert_at = first_cargo_subcommand_index(args).unwrap_or(0);
    out.splice(insert_at..insert_at, cargo_args.iter().cloned());
    out
}

/// Resolve transitive runtime dependencies for `cargo-<sub>` and append
/// their bin directories to `extra_bin_dirs` (PATH-prepended on the
/// child cargo) and any required env overrides to `extra_env`.
///
/// Registered bootstraps:
///   - `cargo zigbuild` → ensure `zig` is on PATH (PR #841).
///   - explicit legacy `cargo zigbuild --target *-apple-darwin` → ensure
///     Apple SDK on disk + set `SDKROOT` env (issue #854).
///   - `cargo xwin build --target *-pc-windows-msvc` → ensure `clang`
///     shim on PATH that forces `--driver-mode=cl` (PR #849).
///   - `cargo nextest archive --target {darwin,windows-msvc}` → reuse the
///     blessed SDK + clang/lld prep from `soldr build` (soldr#1432/#1524).
async fn append_subcommand_transitive_bin_dirs(
    sub: &str,
    args: &[String],
    paths: &SoldrPaths,
    extra_bin_dirs: &mut Vec<std::path::PathBuf>,
    extra_env: &mut Vec<(String, String)>,
    extra_cargo_args: &mut Vec<String>,
) -> Result<(), SoldrError> {
    if sub == "zigbuild" {
        let zig_dir = crate::fetch::ensure_zig(paths).await?;
        extra_bin_dirs.push(zig_dir.clone());
        // Explicit legacy `soldr cargo zigbuild --target *-apple-darwin`
        // needs the Apple SDK on disk + `SDKROOT` exported so cargo-zigbuild's
        // mach-O linker can resolve `-framework IOKit` / etc.
        // Without this, every Rust dep with an Apple-framework
        // dependency (ring, sysinfo, dirs, …) fails to link.
        if let Some(triple) = extract_target_arg(args) {
            append_zigbuild_env_overrides(paths, triple, extra_env)?;
            if triple.ends_with("-apple-darwin") {
                let sdk_dir = crate::fetch::ensure_apple_sdk(paths, Some(triple)).await?;
                // Don't clobber a caller-set SDKROOT — escape hatch
                // for users with their own Xcode SDK or a custom one.
                if std::env::var_os("SDKROOT").is_none() {
                    extra_env.push((
                        "SDKROOT".to_string(),
                        sdk_dir.to_string_lossy().into_owned(),
                    ));
                }
                if std::env::var_os("PKG_CONFIG_SYSROOT_DIR").is_none() {
                    extra_env.push((
                        "PKG_CONFIG_SYSROOT_DIR".to_string(),
                        sdk_dir.to_string_lossy().into_owned(),
                    ));
                }
            }
        }
    }
    // `soldr cargo xwin build --target *-pc-windows-msvc` needs a
    // `clang` shim that forces `--driver-mode=cl`. ring's build.rs
    // hard-codes `c.compiler("clang")` for the aarch64 target which
    // overrides cc-rs's env-driven compiler choice (so just setting
    // CC_<triple>=clang-cl doesn't help). Putting our shim on PATH
    // wins ring's `clang` PATH lookup. See `clang_cl_shim` for the
    // full rationale.
    //
    // In addition, the same lane needs a real LLVM toolchain on PATH
    // — clang-cl / lld-link / llvm-lib — for cargo-xwin's link step
    // to succeed on a stock Linux runner that doesn't have `apt install
    // llvm clang lld` baked in. soldr fetches the toolchain from
    // `zackees/clang-tool-chain-bins` (closes #855, sub of meta #853)
    // and prepends its `bin/` to PATH alongside the clang shim. Env
    // overrides for CC_<triple> / CXX_<triple> / AR_<triple> / LD_<triple>
    // are set to absolute paths inside the fetched bin dir so cc-rs
    // and rustc can find the right driver even if PATH ordering shifts.
    //
    // On hosts not in the managed LLVM matrix (today: macOS), we log
    // and skip — those hosts ship Apple's `clang` via Xcode, and the
    // xwin lane is not a primary mac flow. Workflow-side YAML hedges
    // (apt-installed llvm/clang/lld) remain in place; sub-issue #857
    // removes them once this auto-bootstrap proves out across the
    // matrix.
    if let Some(triple) = nextest_archive_zig_target(args) {
        let zig_dir = crate::fetch::ensure_zig(paths).await?;
        extra_bin_dirs.push(zig_dir);
        append_zigbuild_env_overrides(paths, triple, extra_env)?;
    }
    if sub == "xwin" {
        if let Some(triple) = extract_target_arg(args) {
            if triple.ends_with("-pc-windows-msvc") {
                match crate::fetch::ensure_llvm_toolchain(paths).await {
                    Ok(llvm_bin_dir) => {
                        let ext = std::env::consts::EXE_SUFFIX;
                        let clang = llvm_bin_dir.join(format!("clang{ext}"));
                        let clang_cl = llvm_bin_dir.join(format!("clang-cl{ext}"));
                        let llvm_lib = llvm_bin_dir.join(format!("llvm-lib{ext}"));
                        let lld_link = llvm_bin_dir.join(format!("lld-link{ext}"));
                        let shim_dir =
                            clang_cl_shim::ensure_clang_cl_shim_for_real_clang(paths, &clang)?;
                        extra_bin_dirs.push(shim_dir);
                        let suffix = triple.replace('-', "_");
                        // Don't clobber caller-set values — escape hatch
                        // for users who pinned their own LLVM build.
                        // Note: `compute_subcommand_env_overrides` sets
                        // bare names (`clang-cl` / `llvm-lib`) for the
                        // same triple; the absolute paths we set here
                        // win because they're pushed into `extra_env`
                        // and applied AFTER the env loop checks
                        // `var_os().is_none()` (transitive env applies
                        // unconditionally — see the apply loop below).
                        // To avoid that double-set racing, gate on
                        // `var_os` here too: the bare-name fallback
                        // still hits PATH which now contains the
                        // LLVM bin dir.
                        for (key, val) in [
                            (format!("CC_{suffix}"), &clang_cl),
                            (format!("CXX_{suffix}"), &clang_cl),
                            (format!("AR_{suffix}"), &llvm_lib),
                            (format!("LD_{suffix}"), &lld_link),
                        ] {
                            if std::env::var_os(&key).is_none() {
                                extra_env.push((key, val.to_string_lossy().into_owned()));
                            }
                        }
                        extra_bin_dirs.push(llvm_bin_dir);
                    }
                    Err(SoldrError::UnsupportedPlatform(msg)) => {
                        let shim_dir = clang_cl_shim::ensure_clang_cl_shim(paths)?;
                        extra_bin_dirs.push(shim_dir);
                        eprintln!(
                            "soldr: skipping managed LLVM bootstrap: {msg}; \
                             falling back to system clang/lld-link/llvm-lib on PATH"
                        );
                    }
                    Err(err) => return Err(err),
                }

                // zlib-ng's ARM optimizations are unbuildable under
                // clang-cl — chain a toolchain-file wrapper that turns
                // them off for the aarch64 lane. See
                // `ensure_zlib_ng_arm_cmake_wrapper` for the full
                // root-cause writeup (cross-run 28574600982 fix).
                if let Some((key, value)) = ensure_zlib_ng_arm_cmake_wrapper(paths, triple)? {
                    if std::env::var_os(&key).is_none() {
                        extra_env.push((key, value));
                    }
                }
            }
        }
    }
    if let Some(triple) = nextest_archive_blessed_target(args) {
        let prep = crate::blessed_build::prepare(paths, triple).await?;
        if triple.ends_with("-pc-windows-msvc") && prep.xwin_cache_dir.is_none() {
            return Err(SoldrError::Other(format!(
                "cargo nextest archive for {triple} requires the managed xwin-cache; \
                 the blessed toolchain could not materialize it"
            )));
        }
        append_blessed_prep_to_subcommand_bootstrap(
            prep,
            extra_bin_dirs,
            extra_env,
            extra_cargo_args,
        );
    }
    Ok(())
}

fn nextest_archive_blessed_target(args: &[String]) -> Option<&str> {
    let sub_idx = first_cargo_subcommand_index(args)?;
    if args[sub_idx] != "nextest" {
        return None;
    }
    if first_nextest_verb(args, sub_idx) != Some("archive") {
        return None;
    }
    let triple = extract_target_arg(args)?;
    (triple.ends_with("-apple-darwin") || triple.ends_with("-pc-windows-msvc")).then_some(triple)
}

fn nextest_archive_zig_target(args: &[String]) -> Option<&str> {
    let sub_idx = first_cargo_subcommand_index(args)?;
    if args[sub_idx] != "nextest" || first_nextest_verb(args, sub_idx) != Some("archive") {
        return None;
    }
    extract_target_arg(args).filter(|triple| is_zig_linux_cross_target(triple))
}

fn is_zig_linux_cross_target(triple: &str) -> bool {
    triple.ends_with("-unknown-linux-musl") || triple == "aarch64-unknown-linux-gnu"
}

fn zig_cross_target(args: &[String]) -> Option<&str> {
    if let Some(target) = nextest_archive_zig_target(args) {
        return Some(target);
    }
    let sub_idx = first_cargo_subcommand_index(args)?;
    (args[sub_idx] == "zigbuild")
        .then(|| extract_target_arg(args))
        .flatten()
        .filter(|target| is_zig_linux_cross_target(target))
}

fn emit_zig_cross_linker_preflight(
    command: &std::process::Command,
    args: &[String],
) -> Result<(), SoldrError> {
    let Some(target) = zig_cross_target(args) else {
        return Ok(());
    };
    let key = format!(
        "CARGO_TARGET_{}_LINKER",
        target.replace('-', "_").to_ascii_uppercase()
    );
    let linker = command
        .get_envs()
        .find_map(|(name, value)| (name == std::ffi::OsStr::new(&key)).then_some(value))
        .flatten()
        .map(std::ffi::OsStr::to_os_string)
        .or_else(|| std::env::var_os(&key));
    validate_zig_cross_linker(target, linker.as_deref())?;
    eprintln!(
        "soldr: cross-link preflight requested_target={target} effective_target={target} artifact_target={target} linker={} env={key} status=ok",
        linker.as_deref().unwrap_or_default().to_string_lossy()
    );
    Ok(())
}

fn validate_zig_cross_linker(
    target: &str,
    linker: Option<&std::ffi::OsStr>,
) -> Result<(), SoldrError> {
    let linker = linker.ok_or_else(|| {
        SoldrError::Other(format!(
            "cross-link preflight failed for {target}: target linker is unset"
        ))
    })?;
    let path = std::path::Path::new(linker);
    if path.components().count() == 1 {
        let name = linker.to_string_lossy().to_ascii_lowercase();
        let name = name.strip_suffix(".exe").unwrap_or(&name);
        let host_fallback = matches!(name, "cc" | "gcc" | "clang" | "ld")
            || name
                .strip_prefix("clang-")
                .is_some_and(|version| version.chars().all(|ch| ch.is_ascii_digit()));
        if host_fallback {
            return Err(SoldrError::Other(format!(
                "cross-link preflight failed for {target}: `{}` is a bare host linker; configure the target-scoped Zig/cross linker before compiling target objects",
                linker.to_string_lossy()
            )));
        }
    }
    Ok(())
}

fn first_nextest_verb(args: &[String], nextest_idx: usize) -> Option<&str> {
    let mut skip_next = false;
    for arg in args.iter().skip(nextest_idx + 1) {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "--" {
            return None;
        }
        if nextest_global_arg_takes_value(arg) {
            skip_next = !arg.contains('=');
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        return Some(arg.as_str());
    }
    None
}

fn nextest_global_arg_takes_value(arg: &str) -> bool {
    matches!(arg, "--color" | "--config-file" | "--tool-config-file")
        || arg.starts_with("--color=")
        || arg.starts_with("--config-file=")
        || arg.starts_with("--tool-config-file=")
}

fn append_blessed_prep_to_subcommand_bootstrap(
    prep: crate::blessed_build::BlessedPrep,
    extra_bin_dirs: &mut Vec<std::path::PathBuf>,
    extra_env: &mut Vec<(String, String)>,
    extra_cargo_args: &mut Vec<String>,
) {
    extra_bin_dirs.extend(prep.path_prefix());
    extra_env.extend(prep.env);
    extra_cargo_args.extend(prep.cargo_args);
}

/// Write the cmake toolchain-file WRAPPER that disables zlib-ng's ARM
/// optimizations for `aarch64-pc-windows-msvc` cross-compiles, and
/// return the `(env var, path)` pair to export on the child cargo.
/// Returns `Ok(None)` for every other triple.
///
/// ## Why (run 28574600982, windows-arm lane)
///
/// libz-ng-sys builds vendored zlib-ng via cmake. Under clang-cl
/// (`_MSC_VER` defined, but none of MSVC's compiler intrinsics exist),
/// zlib-ng's ARM feature detection is self-INCONSISTENT:
///
/// * `HAVE_ARMV8_INTRIN` probes `__crc32w` from `<intrin.h>` — an
///   MSVC-only declaration clang-cl doesn't ship → probe fails. But
///   the GNU-asm probe `HAVE_ARMV8_INLINE_ASM` passes, so cmake
///   enables `ARM_CRC32` anyway — and `acle_intrins.h`'s `_MSC_VER`
///   code path then requires exactly the missing `__crc32b/h/w/d`
///   intrinsics: `crc32_armv8.c` fails with "call to undeclared
///   function '__crc32b'".
/// * The NEON path includes MSVC's `arm64_neon.h`, whose
///   `vld1q_*_x4` macros expand to `neon_ld1m4_*` compiler magic that
///   clang-cl neither declares nor lowers. The `NEON_HAS_LD4` probe
///   dies at link ("undefined symbol: neon_ld1m4_q32"), and the
///   fallback inline functions zlib-ng then compiles collide with the
///   still-defined macros ("type specifier missing" in
///   `adler32_neon.c` via `neon_intrins.h`).
///
/// Turning `WITH_NEON` / `WITH_ARMV8` / `WITH_ARMV6` off makes
/// zlib-ng build its portable C fallbacks — the only combination
/// clang-cl can actually compile until upstream zlib-ng grows real
/// clang-cl ARM64 support.
///
/// ## How the wrapper reaches cmake
///
/// cargo-xwin exports `CMAKE_TOOLCHAIN_FILE_<underscore-triple>` on
/// the child cargo. The `cmake` crate (used by libz-ng-sys's
/// build.rs) checks the DASH-triple form of that variable FIRST
/// (`getenv_target_os` in cmake-rs), so soldr exports
/// `CMAKE_TOOLCHAIN_FILE_aarch64-pc-windows-msvc` pointing at this
/// wrapper. The wrapper chain-includes cargo-xwin's real clang-cl
/// toolchain file via `$ENV{...}` (still present in the build-script
/// environment) so compiler/linker setup is byte-identical, then
/// force-caches the three `WITH_*` toggles.
fn ensure_zlib_ng_arm_cmake_wrapper(
    paths: &SoldrPaths,
    triple: &str,
) -> Result<Option<(String, String)>, SoldrError> {
    if !(triple.starts_with("aarch64-") && triple.ends_with("-pc-windows-msvc")) {
        return Ok(None);
    }
    let dir = paths.root.join("cmake").join(triple);
    std::fs::create_dir_all(&dir).map_err(|e| {
        SoldrError::Other(format!("create cmake wrapper dir {}: {e}", dir.display()))
    })?;
    let wrapper = dir.join("clang-cl-arm-toolchain.cmake");
    let underscore = triple.replace('-', "_");
    let content = format!(
        r#"# Written by soldr (cross-run 28574600982 fix) — do not edit; regenerated each run.
# Chain-include cargo-xwin's generated clang-cl toolchain file (it
# exports the underscore-form env var on the child cargo) so the
# compiler/linker setup is unchanged.
if(DEFINED ENV{{CMAKE_TOOLCHAIN_FILE_{underscore}}})
    include("$ENV{{CMAKE_TOOLCHAIN_FILE_{underscore}}}")
endif()

# zlib-ng's ARM optimizations require MSVC-only compiler intrinsics
# (__crc32*, neon_ld1m4_*) that clang-cl does not implement; its cmake
# feature detection half-enables them anyway and the build dies in
# crc32_armv8.c / adler32_neon.c. Force the portable C fallbacks.
set(WITH_NEON OFF CACHE BOOL "soldr: MSVC-intrinsic-only under clang-cl" FORCE)
set(WITH_ARMV8 OFF CACHE BOOL "soldr: MSVC-intrinsic-only under clang-cl" FORCE)
set(WITH_ARMV6 OFF CACHE BOOL "soldr: MSVC-intrinsic-only under clang-cl" FORCE)
"#
    );
    std::fs::write(&wrapper, content).map_err(|e| {
        SoldrError::Other(format!("write cmake wrapper {}: {e}", wrapper.display()))
    })?;
    Ok(Some((
        format!("CMAKE_TOOLCHAIN_FILE_{triple}"),
        wrapper.to_string_lossy().into_owned(),
    )))
}

fn append_zigbuild_env_overrides(
    paths: &SoldrPaths,
    triple: &str,
    extra_env: &mut Vec<(String, String)>,
) -> Result<(), SoldrError> {
    let wrappers = match zig_shim::ensure_zig_wrappers(paths, triple) {
        Ok(wrappers) => wrappers,
        Err(SoldrError::UnsupportedPlatform(_)) => return Ok(()),
        Err(err) => return Err(err),
    };
    let suffix = triple.replace('-', "_");
    let upper = suffix.to_uppercase();
    for (key, val) in [
        (format!("CC_{suffix}"), wrappers.cc.as_path()),
        (format!("CXX_{suffix}"), wrappers.cxx.as_path()),
        (format!("AR_{suffix}"), wrappers.ar.as_path()),
        (format!("RANLIB_{suffix}"), wrappers.ranlib.as_path()),
        (
            format!("CARGO_TARGET_{upper}_LINKER"),
            wrappers.cc.as_path(),
        ),
    ] {
        if std::env::var_os(&key).is_none() {
            extra_env.push((key, val.to_string_lossy().into_owned()));
        }
    }
    Ok(())
}

/// Compute environment-variable overrides for the child cargo invocation
/// based on the subcommand and its `--target` argument.
///
/// The only rule today: `cargo xwin build --target <triple>` for any
/// `<triple>` ending in `-pc-windows-msvc` injects
///
///   CC_<triple-underscored>  = clang-cl
///   CXX_<triple-underscored> = clang-cl
///   AR_<triple-underscored>  = llvm-lib
///
/// Why: cc-rs (used by ring, blake3, and other C-FFI crates) detects
/// `target=*-pc-windows-msvc` and formats include flags MSVC-style
/// (`/imsvc <path>`). But cc-rs's default driver for that triple,
/// when `cl.exe` isn't on PATH (typical on Linux cross-compile hosts),
/// is the GNU-flavoured `clang` driver — which interprets `/imsvc`
/// as a literal filename. The result is the build.rs error
///   `clang: error: no such file or directory: '/imsvc'`
/// observed when cross-compiling soldr to windows-arm64 via cargo-xwin
/// on a linux runner.
///
/// The fix is to tell cc-rs to use `clang-cl` (clang's MSVC-compatible
/// driver) explicitly. cc-rs reads `CC_<triple-underscored>` ahead of
/// its default heuristic; setting it routes ring's assembly compilation
/// to the right driver and the build succeeds.
///
/// The caller's existing `CC_*` / `CXX_*` / `AR_*` env vars take
/// precedence (the apply loop checks `std::env::var_os` first); this
/// hook only fills in the gaps.
fn compute_subcommand_env_overrides(args: &[String]) -> Vec<(String, String)> {
    let Some(sub) = first_cargo_subcommand(args) else {
        return Vec::new();
    };
    if sub != "xwin" {
        return Vec::new();
    }
    // Verify the inner verb is `build` / `test` / etc. (anything cc-rs
    // would invoke a build script for). cargo-xwin's other verbs
    // (`download`, `pre-download`) don't compile anything.
    let mut after_sub = args.iter().skip_while(|a| a.as_str() != sub);
    after_sub.next(); // consume the matched `xwin`
    let needs_cc = matches!(
        after_sub.clone().next().map(String::as_str),
        Some("build" | "test" | "check" | "run" | "bench" | "doc" | "clippy" | "rustc"),
    );
    if !needs_cc {
        return Vec::new();
    }
    let Some(triple) = extract_target_arg(args) else {
        return Vec::new();
    };
    if !triple.ends_with("-pc-windows-msvc") {
        return Vec::new();
    }
    let suffix = triple.replace('-', "_");
    vec![
        (format!("CC_{suffix}"), "clang-cl".to_string()),
        (format!("CXX_{suffix}"), "clang-cl".to_string()),
        (format!("AR_{suffix}"), "llvm-lib".to_string()),
    ]
}

/// Find the value of `--target <triple>` or `--target=<triple>` in a
/// cargo arg vector. Returns `None` if the arg isn't present. Used by
/// `compute_subcommand_env_overrides` to decide whether to inject
/// MSVC-target cc-rs env vars.
fn extract_target_arg(args: &[String]) -> Option<&str> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--target" {
            return it.next().map(String::as_str);
        }
        if let Some(rest) = a.strip_prefix("--target=") {
            return Some(rest);
        }
    }
    None
}

#[cfg(test)]
mod tests;
