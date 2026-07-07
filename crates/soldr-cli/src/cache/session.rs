//! `soldr session start/end`, `soldr cache shutdown`, and `soldr cache flush` —
//! lifecycle commands for the zccache daemon and its per-session journal.

use crate::core::{SoldrError, SoldrPaths};
use crate::daemon::protocol::CompileStatsInfo;
use crate::zccache::managed_zccache_cache_dir;
use crate::JSON_SCHEMA_VERSION;
use serde::Serialize;

#[derive(Serialize)]
struct SessionStartOutput {
    schema_version: u32,
    command: &'static str,
    session_id: String,
    log_path: String,
    journal_path: String,
    stats_path: String,
    cache_dir: String,
    /// True when the caller already had a session in
    /// `ZCCACHE_SESSION_ID` and we did not contact the daemon. soldr#379
    /// requires session-start be idempotent for the calling process.
    reused: bool,
}

#[derive(Serialize)]
struct SessionEndOutput {
    schema_version: u32,
    command: &'static str,
    session_id: String,
    /// Parsed contents of `zccache session-end --json`. `null` when the
    /// session was already gone (a second session-end is a no-op per
    /// the soldr#379 idempotency contract).
    stats: Option<serde_json::Value>,
    /// True when this call was a no-op because the named session had
    /// already been finalized.
    already_ended: bool,
    /// True when the caller passed `--clear` and journal/log/stats
    /// files were removed from disk.
    cleared: bool,
}

#[derive(Serialize)]
struct CacheShutdownOutput {
    schema_version: u32,
    command: &'static str,
    cache_dir: String,
    /// The session that was finalized (if any) before stopping the
    /// daemon.
    session_id: Option<String>,
    /// Whether a graceful `zccache stop` ran. False when the daemon was
    /// already stopped or no zccache binary has ever been fetched.
    daemon_stopped: bool,
    /// Whether the daemon process was observed to have actually exited
    /// after `zccache stop` (per soldr#383, `zccache stop` returns
    /// before the daemon process has exited, so soldr polls
    /// `zccache status` until the daemon stops responding).
    /// `false` when the daemon was never running, when polling was
    /// disabled with `--no-wait`, or when polling timed out.
    daemon_exited: bool,
    /// Where session logs were archived to, if `--archive-logs` was
    /// supplied.
    archive_dir: Option<String>,
    /// Diagnostic notes for the human-facing print path.
    notes: Vec<String>,
}

#[derive(Serialize)]
struct CacheFlushOutput {
    schema_version: u32,
    command: &'static str,
    cache_dir: String,
    /// True after `zccache flush` returned 0. False when zccache does
    /// not yet support the `flush` subcommand or when the daemon was
    /// never running.
    flushed: bool,
    /// Parsed contents of `zccache flush --json` stdout when zccache
    /// emits it. `null` when zccache only prints a text summary or
    /// when flush was not run.
    stats: Option<serde_json::Value>,
    /// Diagnostic notes for the human-facing print path.
    notes: Vec<String>,
}

pub(crate) async fn run_session_start_command(
    id: Option<String>,
    log: Option<std::path::PathBuf>,
    journal: Option<std::path::PathBuf>,
    json: bool,
) -> Result<(), SoldrError> {
    let paths = SoldrPaths::new()?;
    let zccache_dir = managed_zccache_cache_dir(&paths)?;
    std::fs::create_dir_all(&zccache_dir)?;
    std::fs::create_dir_all(zccache_dir.join("logs"))?;

    let session_log_path = log.unwrap_or_else(|| crate::cache_lib::session_log_path(&zccache_dir));
    let journal_path =
        journal.unwrap_or_else(|| crate::cache_lib::session_journal_path(&zccache_dir));
    let session_stats_path = crate::cache_lib::session_stats_path(&zccache_dir);

    // Idempotent path: if ZCCACHE_SESSION_ID is already set and the
    // caller did not pass an explicit --id, reuse the existing session
    // without contacting the daemon. soldr#379 requires this so
    // setup-soldr can call session-start repeatedly without spawning
    // orphan sessions.
    if id.is_none() {
        if let Ok(existing) = std::env::var(crate::cache_lib::ZCCACHE_SESSION_ID_ENV_VAR) {
            let trimmed = existing.trim();
            if !trimmed.is_empty() {
                let output = SessionStartOutput {
                    schema_version: JSON_SCHEMA_VERSION,
                    command: "session-start",
                    session_id: trimmed.to_string(),
                    log_path: session_log_path.display().to_string(),
                    journal_path: journal_path.display().to_string(),
                    stats_path: session_stats_path.display().to_string(),
                    cache_dir: zccache_dir.display().to_string(),
                    reused: true,
                };
                emit_session_start(&output, json)?;
                return Ok(());
            }
        }
    }

    // soldr#1368: sessions are no longer external zccache-daemon sessions.
    // Mint a local id and capture a baseline of the soldr-daemon embedded
    // zccache service's cumulative compile counters so `soldr session end`
    // can diff against it. Best-effort: if the daemon isn't up yet, record
    // a zero baseline (this session's compiles all count against it).
    let session_id = id.unwrap_or_else(mint_session_id);
    let baseline = embedded_compile_stats(&paths).unwrap_or_default();
    write_session_baseline(&zccache_dir, &session_id, &baseline)?;

    let output = SessionStartOutput {
        schema_version: JSON_SCHEMA_VERSION,
        command: "session-start",
        session_id,
        log_path: session_log_path.display().to_string(),
        journal_path: journal_path.display().to_string(),
        stats_path: session_stats_path.display().to_string(),
        cache_dir: zccache_dir.display().to_string(),
        reused: false,
    };
    emit_session_start(&output, json)?;
    Ok(())
}

/// Generate a local build-session id (soldr#1368). No longer a zccache
/// daemon session id — a blake3 of (pid, monotonic nanos) suffices.
fn mint_session_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut hasher = zccache::hash::StreamHasher::new();
    hasher.update(&std::process::id().to_le_bytes());
    hasher.update(&nanos.to_le_bytes());
    hex::encode(&hasher.finalize().as_bytes()[..12])
}

/// Query the soldr-daemon embedded zccache service for its cumulative
/// compile counters. `None` when the daemon is not reachable.
fn embedded_compile_stats(paths: &SoldrPaths) -> Option<CompileStatsInfo> {
    let sock = crate::daemon::server::server_sock_path(paths);
    crate::daemon::client::compile_stats(&sock).ok()
}

fn session_baseline_path(zccache_dir: &std::path::Path, session_id: &str) -> std::path::PathBuf {
    zccache_dir
        .join("logs")
        .join(format!("session-{session_id}.baseline.json"))
}

fn write_session_baseline(
    zccache_dir: &std::path::Path,
    session_id: &str,
    stats: &CompileStatsInfo,
) -> Result<(), SoldrError> {
    let path = session_baseline_path(zccache_dir, session_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string(stats)
        .map_err(|e| SoldrError::Other(format!("failed to serialize session baseline: {e}")))?;
    std::fs::write(&path, json)?;
    Ok(())
}

fn read_session_baseline(
    zccache_dir: &std::path::Path,
    session_id: &str,
) -> Option<CompileStatsInfo> {
    let raw = std::fs::read_to_string(session_baseline_path(zccache_dir, session_id)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// soldr#1368 observability restore — build-start half. Snapshot the
/// embedded zccache compile counters (via the soldr-daemon) so
/// [`finalize_build_session_stats`] can diff them into per-build
/// hit/miss figures. Best-effort: if the daemon isn't up yet no baseline
/// is written, and the finalize step then treats the baseline as all
/// zero — correct because a daemon that starts fresh for this build has
/// cumulative counters equal to this build's stats.
pub(crate) fn capture_build_baseline(zccache_dir: &std::path::Path, session_id: &str) {
    let Ok(paths) = SoldrPaths::new() else {
        return;
    };
    if let Some(baseline) = embedded_compile_stats(&paths) {
        let _ = write_session_baseline(zccache_dir, session_id, &baseline);
    }
}

/// soldr#1368 observability restore — build-end half. Diff the embedded
/// zccache compile counters against the build-start baseline and write
/// the per-build hit/miss summary to `last-session-stats.json`, the
/// artifact `soldr cache report` (and the perf harness) read. Restores
/// the reporting the pre-#1368 managed `zccache session-end` path used
/// to produce. A missing baseline is treated as all-zero (fresh daemon).
/// No-op when the daemon is unreachable at end (nothing to report).
pub(crate) fn finalize_build_session_stats(zccache_dir: &std::path::Path, session_id: &str) {
    let Ok(paths) = SoldrPaths::new() else {
        return;
    };
    let Some(current) = embedded_compile_stats(&paths) else {
        return;
    };
    let baseline = read_session_baseline(zccache_dir, session_id).unwrap_or_default();
    if let Some(stats) = compute_session_stats(Some(&baseline), Some(&current)) {
        let path = crate::cache_lib::session_stats_path(zccache_dir);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string(&stats) {
            let _ = std::fs::write(&path, json);
        }
    }
    let _ = std::fs::remove_file(session_baseline_path(zccache_dir, session_id));
}

/// Diff a session-end stats snapshot against the session-start baseline
/// into per-session `hits`/`misses`/`hit_rate`/… JSON. `None` when the
/// daemon was unreachable at end (no current snapshot to diff).
fn compute_session_stats(
    baseline: Option<&CompileStatsInfo>,
    current: Option<&CompileStatsInfo>,
) -> Option<serde_json::Value> {
    let (base, cur) = (baseline?, current?);
    let hits = cur.cache_hits.saturating_sub(base.cache_hits);
    let misses = cur.cache_misses.saturating_sub(base.cache_misses);
    let non_cacheable = cur.non_cacheable.saturating_sub(base.non_cacheable);
    let errors = cur.compile_errors.saturating_sub(base.compile_errors);
    let compilations = cur
        .total_compilations
        .saturating_sub(base.total_compilations);
    let time_saved_ms = cur.time_saved_ms.saturating_sub(base.time_saved_ms);
    let denom = hits + misses;
    let hit_rate = if denom > 0 {
        hits as f64 / denom as f64
    } else {
        0.0
    };
    Some(serde_json::json!({
        "status": "ok",
        "hits": hits,
        "misses": misses,
        "non_cacheable": non_cacheable,
        "errors": errors,
        "compilations": compilations,
        "time_saved_ms": time_saved_ms,
        "hit_rate": hit_rate,
    }))
}

fn emit_session_start(output: &SessionStartOutput, json: bool) -> Result<(), SoldrError> {
    if json {
        // Single-line JSON keeps the output greppable by shell consumers
        // and `actions/core` parsers; see soldr#379 contract.
        let line = serde_json::to_string(output)
            .map_err(|e| SoldrError::Other(format!("failed to serialize session-start: {e}")))?;
        println!("{line}");
    } else {
        println!("ZCCACHE_SESSION_ID={}", output.session_id);
        println!("log: {}", output.log_path);
        println!("journal: {}", output.journal_path);
        println!("stats: {}", output.stats_path);
        if output.reused {
            println!("(reused existing session via ZCCACHE_SESSION_ID env var)");
        }
    }
    Ok(())
}

pub(crate) fn run_session_end_command(
    id: Option<String>,
    clear: bool,
    json: bool,
) -> Result<(), SoldrError> {
    let session_id = match id.or_else(|| {
        std::env::var(crate::cache_lib::ZCCACHE_SESSION_ID_ENV_VAR)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    }) {
        Some(value) => value,
        None => {
            return Err(SoldrError::Other(format!(
                "session-end requires --id or ${} to be set",
                crate::cache_lib::ZCCACHE_SESSION_ID_ENV_VAR
            )));
        }
    };

    let paths = SoldrPaths::new()?;
    let zccache_dir = managed_zccache_cache_dir(&paths)?;

    // soldr#1368: diff the embedded zccache compile counters against the
    // baseline captured at `session start`. A missing baseline means the
    // session was never started here (or was already ended — the baseline
    // is consumed on end), so this is a no-op idempotent second call.
    let baseline = read_session_baseline(&zccache_dir, &session_id);
    let already_ended = baseline.is_none();
    let stats = if already_ended {
        None
    } else {
        let current = embedded_compile_stats(&paths);
        compute_session_stats(baseline.as_ref(), current.as_ref())
    };
    // Consume the baseline so a second `session end` is a clean no-op
    // (soldr#379 idempotency contract).
    let _ = std::fs::remove_file(session_baseline_path(&zccache_dir, &session_id));

    let cleared = if clear {
        clear_session_artifacts(&zccache_dir)?
    } else {
        false
    };

    let output = SessionEndOutput {
        schema_version: JSON_SCHEMA_VERSION,
        command: "session-end",
        session_id: session_id.clone(),
        stats,
        already_ended,
        cleared,
    };

    if json {
        let line = serde_json::to_string(&output)
            .map_err(|e| SoldrError::Other(format!("failed to serialize session-end: {e}")))?;
        println!("{line}");
    } else {
        println!("session-end: {}", session_id);
        if output.already_ended {
            println!("  status: already-ended (no-op)");
        } else if let Some(stats) = &output.stats {
            if let Some(hits) = stats.get("hits").and_then(|v| v.as_u64()) {
                let misses = stats.get("misses").and_then(|v| v.as_u64()).unwrap_or(0);
                println!("  hits/misses: {hits}/{misses}");
            }
            if let Some(rate) = stats.get("hit_rate").and_then(|v| v.as_f64()) {
                println!("  hit rate: {:.1}%", rate * 100.0);
            }
        }
        if cleared {
            println!("  cleared: journal/log/stats removed");
        }
    }
    Ok(())
}

fn clear_session_artifacts(zccache_dir: &std::path::Path) -> Result<bool, SoldrError> {
    let mut removed_any = false;
    for path in [
        crate::cache_lib::session_journal_path(zccache_dir),
        crate::cache_lib::session_log_path(zccache_dir),
        crate::cache_lib::session_stats_path(zccache_dir),
    ] {
        match std::fs::remove_file(&path) {
            Ok(()) => removed_any = true,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(SoldrError::Other(format!(
                    "failed to remove {}: {err}",
                    path.display()
                )));
            }
        }
    }
    Ok(removed_any)
}

pub(crate) async fn run_cache_shutdown_command(
    archive_logs: Option<std::path::PathBuf>,
    _no_depgraph_save: bool,
    _shutdown_timeout_seconds: u64,
    _wait: bool,
    json: bool,
) -> Result<(), SoldrError> {
    // soldr#1368: there is no separate managed zccache daemon to stop or
    // poll. Durability of the soldr-daemon embedded zccache state is the
    // real soldr#383 guarantee, delivered by FlushCaches. The legacy
    // managed-daemon knobs (`--no-depgraph-save` / `--wait` / timeout) are
    // accepted for CLI compatibility but no longer drive an external stop.
    let paths = SoldrPaths::new()?;
    let zccache_dir = managed_zccache_cache_dir(&paths)?;
    let mut notes: Vec<String> = Vec::new();

    let mut output = CacheShutdownOutput {
        schema_version: JSON_SCHEMA_VERSION,
        command: "cache shutdown",
        cache_dir: zccache_dir.display().to_string(),
        session_id: None,
        daemon_stopped: false,
        daemon_exited: false,
        archive_dir: None,
        notes: Vec::new(),
    };

    let env_session_id = std::env::var(crate::cache_lib::ZCCACHE_SESSION_ID_ENV_VAR)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());

    // End the active session (consume its stats baseline) so a following
    // `session end` is a clean no-op.
    if let Some(session_id) = env_session_id.as_deref() {
        let _ = std::fs::remove_file(session_baseline_path(&zccache_dir, session_id));
        output.session_id = Some(session_id.to_string());
    }

    // Archive the session log/journal/stats into a per-session subdir.
    if let Some(archive_root) = archive_logs.as_ref() {
        let session_id = output
            .session_id
            .clone()
            .or_else(|| env_session_id.clone())
            .unwrap_or_else(|| "no-session".to_string());
        let target = archive_root.join(&session_id);
        std::fs::create_dir_all(&target)?;
        let mut copied = 0u32;
        for path in [
            crate::cache_lib::session_log_path(&zccache_dir),
            crate::cache_lib::session_journal_path(&zccache_dir),
            crate::cache_lib::session_stats_path(&zccache_dir),
        ] {
            if !path.exists() {
                continue;
            }
            if let Some(filename) = path.file_name() {
                std::fs::copy(&path, target.join(filename))?;
                copied += 1;
            }
        }
        output.archive_dir = Some(target.display().to_string());
        notes.push(format!(
            "archived {copied} session file(s) to {}",
            target.display()
        ));
    }

    // Make the embedded zccache state durable on disk (soldr#383 / #1286).
    let sock = crate::daemon::server::server_sock_path(&paths);
    match crate::daemon::client::flush_caches(&sock) {
        Ok(()) => {
            output.daemon_stopped = true;
            output.daemon_exited = true;
            notes.push("embedded zccache state flushed via soldr-daemon".into());
        }
        Err(err) => notes.push(format!(
            "soldr-daemon embedded flush unavailable ({err:?}); on-disk state is \
             whatever the daemon last persisted"
        )),
    }

    output.notes = notes;

    if json {
        let line = serde_json::to_string(&output)
            .map_err(|e| SoldrError::Other(format!("failed to serialize cache shutdown: {e}")))?;
        println!("{line}");
    } else {
        println!("soldr cache shutdown");
        if let Some(session_id) = &output.session_id {
            println!("  session-end: {session_id}");
        }
        if let Some(archive) = &output.archive_dir {
            println!("  archive: {archive}");
        }
        println!(
            "  embedded zccache: {}",
            if output.daemon_stopped {
                "flushed"
            } else {
                "flush unavailable"
            }
        );
        for note in &output.notes {
            println!("  note: {note}");
        }
    }
    Ok(())
}

pub(crate) async fn run_cache_flush_command(json: bool) -> Result<(), SoldrError> {
    let paths = SoldrPaths::new()?;
    let zccache_dir = managed_zccache_cache_dir(&paths)?;
    let mut output = CacheFlushOutput {
        schema_version: JSON_SCHEMA_VERSION,
        command: "cache flush",
        cache_dir: zccache_dir.display().to_string(),
        flushed: false,
        stats: None,
        notes: Vec::new(),
    };

    // soldr#1368: flush the soldr-daemon embedded zccache state to disk
    // (artifact index, depgraph snapshot, metadata cache) so archives taken
    // afterwards restore with warm rustc hits. There is no separate managed
    // zccache daemon to flush any more.
    let sock = crate::daemon::server::server_sock_path(&paths);
    match crate::daemon::client::flush_caches(&sock) {
        Ok(()) => {
            output.flushed = true;
            output
                .notes
                .push("embedded zccache state flushed via soldr-daemon".into());
        }
        Err(err) => output.notes.push(format!(
            "soldr-daemon embedded flush unavailable ({err:?}); on-disk state is \
             whatever the daemon last persisted"
        )),
    }

    if json {
        let line = serde_json::to_string(&output)
            .map_err(|e| SoldrError::Other(format!("failed to serialize cache flush: {e}")))?;
        println!("{line}");
    } else {
        println!("soldr cache flush");
        println!(
            "  status: {}",
            if output.flushed { "flushed" } else { "no-op" }
        );
        for note in &output.notes {
            println!("  note: {note}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::report::zccache_analyze_failure_note;
    use super::super::{
        zccache_daemon_already_stopped, zccache_output_snippet, ZCCACHE_ANALYZE_NOTE_LIMIT,
    };
    use super::clear_session_artifacts;
    use crate::zccache_lifecycle::{zccache_flag_unsupported, zccache_session_already_ended};
    use std::process::Output;

    fn synthetic_output(stderr: &str, exit: i32) -> Output {
        #[cfg(unix)]
        let status = {
            use std::os::unix::process::ExitStatusExt;
            std::process::ExitStatus::from_raw((exit & 0xFF) << 8)
        };
        #[cfg(windows)]
        let status = {
            use std::os::windows::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(exit as u32)
        };
        Output {
            status,
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn session_already_ended_detects_common_error_phrases() {
        for needle in [
            "session not found",
            "Session Not Found: foo",
            "no such session: 123",
            "ALREADY ENDED",
            "unknown session abc",
        ] {
            assert!(
                zccache_session_already_ended(&synthetic_output(needle, 1)),
                "expected {needle:?} to be classified as already-ended"
            );
        }
    }

    #[test]
    fn session_already_ended_rejects_unrelated_errors() {
        for needle in ["compile failure", "permission denied", "internal error"] {
            assert!(
                !zccache_session_already_ended(&synthetic_output(needle, 1)),
                "did not expect {needle:?} to be classified as already-ended"
            );
        }
    }

    #[test]
    fn daemon_already_stopped_detects_common_states() {
        for needle in [
            "daemon not running",
            "No daemon to stop",
            "Connection refused",
            "service not running",
        ] {
            assert!(
                zccache_daemon_already_stopped(&synthetic_output(needle, 1)),
                "expected {needle:?} to indicate daemon already stopped"
            );
        }
    }

    #[test]
    fn flag_unsupported_detects_clap_phrasing() {
        let out = synthetic_output("error: unexpected argument '--no-depgraph-save' found", 2);
        assert!(zccache_flag_unsupported(&out, "--no-depgraph-save"));

        let unrelated = synthetic_output("error: something else went wrong", 1);
        assert!(!zccache_flag_unsupported(&unrelated, "--no-depgraph-save"));
    }

    #[test]
    fn clear_session_artifacts_removes_existing_files_only() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let zccache_dir = tmp.path();
        std::fs::create_dir_all(zccache_dir.join("logs")).expect("logs dir");

        // Only the log file exists; the journal and stats files are absent.
        let log = crate::cache_lib::session_log_path(zccache_dir);
        std::fs::write(&log, b"hello").expect("write log");

        let removed = clear_session_artifacts(zccache_dir).expect("clear");
        assert!(removed, "expected at least one file to be removed");
        assert!(!log.exists(), "log file should be gone");

        // Calling again on an empty state must succeed and report nothing
        // removed (idempotency contract for `session-end --clear`).
        let removed_again = clear_session_artifacts(zccache_dir).expect("clear-twice");
        assert!(!removed_again, "second clear should be a no-op");
    }

    #[test]
    fn zccache_output_snippet_omits_empty_output() {
        assert_eq!(zccache_output_snippet(b""), None);
        assert_eq!(zccache_output_snippet(b" \n\t "), None);
    }

    #[test]
    fn zccache_output_snippet_compacts_whitespace() {
        assert_eq!(
            zccache_output_snippet(b"  first line\n\nsecond\tline  ").as_deref(),
            Some("first line second line")
        );
    }

    #[test]
    fn zccache_output_snippet_truncates_long_output() {
        let output = "x".repeat(ZCCACHE_ANALYZE_NOTE_LIMIT + 10);
        let snippet = zccache_output_snippet(output.as_bytes()).unwrap();
        assert_eq!(snippet.chars().count(), ZCCACHE_ANALYZE_NOTE_LIMIT + 3);
        assert!(snippet.ends_with("..."));
    }

    #[test]
    fn zccache_analyze_failure_note_includes_stdout_and_stderr() {
        let note = zccache_analyze_failure_note(
            Some(1),
            br#"{"status":"error","error":"bad input"}"#,
            b"expected compile journal JSONL\n",
        );
        assert!(note.contains("rollups: zccache analyze exited with status Some(1)"));
        assert!(note.contains("stderr: expected compile journal JSONL"));
        assert!(note.contains(r#"stdout: {"status":"error","error":"bad input"}"#));
    }

    /// soldr#383: the shutdown poll uses a real zccache binary as its
    /// daemon-alive oracle. With a missing binary path, the poll must
    /// terminate quickly with `PollFailed`, never silently loop until
    /// the deadline expires (which would mask the underlying error).
    #[test]
    fn poll_zccache_daemon_exit_surfaces_spawn_failure() {
        use crate::zccache_lifecycle::{ZccacheDaemonExitPollResult, ZccacheLifecycle};
        use std::time::Duration;

        let tmp = tempfile::tempdir().expect("tempdir");
        let bogus = tmp.path().join("definitely-not-zccache");
        let lifecycle = ZccacheLifecycle::new(&bogus, tmp.path());
        let result = lifecycle.poll_daemon_exit(Duration::from_millis(50));
        assert!(
            matches!(result, ZccacheDaemonExitPollResult::PollFailed(_)),
            "expected PollFailed when zccache binary cannot be spawned"
        );
    }
}
