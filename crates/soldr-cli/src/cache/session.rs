//! `soldr session start/end`, `soldr cache shutdown`, and `soldr cache flush` —
//! lifecycle commands for the zccache daemon and its per-session journal.

use crate::core::{SoldrError, SoldrPaths};
use crate::daemon::db;
use crate::zccache::managed_zccache_cache_dir;
use crate::zccache_lifecycle::{
    ZccacheDaemonExitPollResult, ZccacheLifecycle, ZccachePrivateDaemonConfig,
    ZccacheSessionStartOptions,
};
use crate::{cached_active_zccache, fetch_active_zccache, JSON_SCHEMA_VERSION};
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

    let fetch = fetch_active_zccache(&paths).await?;
    let mut lifecycle = ZccacheLifecycle::new(&fetch.binary_path, &zccache_dir);
    let session = lifecycle.start_session(ZccacheSessionStartOptions {
        id,
        session_log_path: session_log_path.clone(),
        journal_path: journal_path.clone(),
        session_stats_path: session_stats_path.clone(),
    })?;

    let output = SessionStartOutput {
        schema_version: JSON_SCHEMA_VERSION,
        command: "session-start",
        session_id: session.session_id,
        log_path: session_log_path.display().to_string(),
        journal_path: journal_path.display().to_string(),
        stats_path: session_stats_path.display().to_string(),
        cache_dir: zccache_dir.display().to_string(),
        reused: false,
    };
    emit_session_start(&output, json)?;
    Ok(())
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
    let fetch = cached_active_zccache(&paths)?.ok_or_else(|| {
        SoldrError::Other(
            "managed zccache binary not yet fetched — run a cache-enabled build first".into(),
        )
    })?;

    let mut lifecycle = ZccacheLifecycle::new(&fetch.binary_path, &zccache_dir);
    let result = lifecycle.end_session_json(&session_id)?;
    let (stats, already_ended) = if result.already_ended {
        (None, true)
    } else {
        let parsed = if result.stdout.trim().is_empty() {
            None
        } else {
            serde_json::from_str::<serde_json::Value>(result.stdout.trim()).ok()
        };
        (parsed, false)
    };

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
    no_depgraph_save: bool,
    shutdown_timeout_seconds: u64,
    wait: bool,
    json: bool,
) -> Result<(), SoldrError> {
    let paths = SoldrPaths::new()?;
    let zccache_dir = managed_zccache_cache_dir(&paths)?;
    let linked_private = linked_private_zccache_lifecycle(&paths);
    let active_zccache_dir = linked_private
        .as_ref()
        .map(|(_, cache_dir)| cache_dir.clone())
        .unwrap_or_else(|| zccache_dir.clone());
    let mut notes: Vec<String> = Vec::new();

    let fetch = cached_active_zccache(&paths)?;
    let mut output = CacheShutdownOutput {
        schema_version: JSON_SCHEMA_VERSION,
        command: "cache shutdown",
        cache_dir: active_zccache_dir.display().to_string(),
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
    let mut lifecycle = linked_private.map(|(lifecycle, _)| lifecycle).or_else(|| {
        fetch
            .as_ref()
            .map(|fetch| ZccacheLifecycle::new(&fetch.binary_path, &zccache_dir))
    });

    // Step 1: end the active session so zccache flushes the per-session
    // journal before the daemon stops.
    if let (Some(session_id), Some(lifecycle)) = (env_session_id.as_deref(), lifecycle.as_mut()) {
        match lifecycle.end_session_json(session_id) {
            Ok(_) => output.session_id = Some(session_id.to_string()),
            Err(err) => notes.push(format!("session-end {session_id} failed: {err}")),
        }
    }

    // Step 2: archive the session log/journal/stats into a per-session
    // subdirectory so future runs do not overwrite the history.
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
            crate::cache_lib::session_log_path(&active_zccache_dir),
            crate::cache_lib::session_journal_path(&active_zccache_dir),
            crate::cache_lib::session_stats_path(&active_zccache_dir),
        ] {
            if !path.exists() {
                continue;
            }
            if let Some(filename) = path.file_name() {
                let dest = target.join(filename);
                std::fs::copy(&path, &dest)?;
                copied += 1;
            }
        }
        output.archive_dir = Some(target.display().to_string());
        notes.push(format!(
            "archived {copied} session file(s) to {}",
            target.display()
        ));
    }

    // Step 3: graceful daemon stop (triggers depgraph flush in zccache
    // 1.8.x). `zccache stop` is synchronous and returns once the daemon
    // process has exited.
    if let Some(lifecycle) = lifecycle.as_mut() {
        let stop_result = lifecycle.stop(no_depgraph_save)?;
        if stop_result.unsupported_no_depgraph_save {
            notes.push(
                "zccache stop does not support --no-depgraph-save; retrying with default flush"
                    .into(),
            );
        }
        if stop_result.stopped {
            output.daemon_stopped = true;
        } else if stop_result.already_stopped {
            notes.push("daemon was already stopped".into());
        } else if let Some(failure) = stop_result.failure {
            notes.push(format!("zccache stop failed: {failure}"));
        }
    } else {
        notes.push("managed zccache binary not yet fetched; nothing to stop".into());
    }

    // Step 4 (soldr#383): block until the daemon process has actually
    // exited. `zccache stop` returns before the OS has reaped the
    // daemon, so without this poll the caller (setup-soldr's post step)
    // races the daemon's still-in-flight depgraph save with its
    // `tar | zstd` of the cache directory. The result reproduced in
    // soldr#383's evidence: zero warm-run cache hits because the
    // depgraph file was never durable on disk by the time the tar
    // started.
    //
    // Poll `zccache status` (the same surface the user runs by hand)
    // every 100ms until it reports the daemon is gone, or until the
    // shutdown deadline elapses. We deliberately use the existing
    // `daemon not running` heuristic so the polling code does not need
    // to know zccache's IPC layout.
    let mut polled_for_exit = false;
    if wait && output.daemon_stopped {
        if let Some(lifecycle) = lifecycle.as_ref() {
            polled_for_exit = true;
            match lifecycle
                .poll_daemon_exit(std::time::Duration::from_secs(shutdown_timeout_seconds))
            {
                ZccacheDaemonExitPollResult::Exited => {
                    output.daemon_exited = true;
                }
                ZccacheDaemonExitPollResult::TimedOut => {
                    notes.push(format!(
                        "daemon did not exit within {shutdown_timeout_seconds}s after `zccache stop`; depgraph state may not be durable on disk"
                    ));
                }
                ZccacheDaemonExitPollResult::PollFailed(err) => {
                    notes.push(format!(
                        "could not confirm daemon exit (polling `zccache status` failed): {err}"
                    ));
                }
            }
        }
    }
    if !wait {
        notes.push("polling disabled via --no-wait; daemon exit not confirmed".into());
    }

    // Step 5 (#1286 F1): the steps above only quiesce the MANAGED
    // zccache daemon (the C/C++ side). The rustc side lives in the
    // soldr-daemon's embedded zccache service, whose artifact index +
    // depgraph were memory-only until a graceful daemon exit — the
    // exact same zero-warm-hit archive race soldr#383 fixed for the
    // managed daemon, one layer up. FlushCaches is synchronous: when
    // it returns, the embedded state is durable and a following
    // `soldr save` / tar sees a complete cache tree.
    {
        let sock = crate::daemon::server::server_sock_path(&paths);
        match crate::daemon::client::flush_caches(&sock) {
            Ok(()) => notes.push("embedded zccache state flushed via soldr-daemon".into()),
            Err(err) => notes.push(format!(
                "soldr-daemon embedded flush unavailable ({err:?}); on-disk \
                 state is whatever the daemon last persisted"
            )),
        }
    }

    output.notes = notes;

    let timed_out = polled_for_exit && !output.daemon_exited;

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
            "  daemon: {}",
            if output.daemon_exited {
                "exited"
            } else if output.daemon_stopped {
                "signalled (exit not confirmed)"
            } else {
                "no-op"
            }
        );
        for note in &output.notes {
            println!("  note: {note}");
        }
    }

    if timed_out {
        // soldr#383: surface a non-zero exit when the daemon outlives
        // the polling deadline so the caller (CI) can fail loud
        // instead of racing the depgraph flush with a `tar`. The
        // human-readable note above already explains what happened.
        return Err(SoldrError::Other(format!(
            "cache shutdown: daemon process did not exit within {shutdown_timeout_seconds}s"
        )));
    }
    Ok(())
}

pub(crate) async fn run_cache_flush_command(json: bool) -> Result<(), SoldrError> {
    let paths = SoldrPaths::new()?;
    let zccache_dir = managed_zccache_cache_dir(&paths)?;
    let linked_private = linked_private_zccache_lifecycle(&paths);
    let active_zccache_dir = linked_private
        .as_ref()
        .map(|(_, cache_dir)| cache_dir.clone())
        .unwrap_or_else(|| zccache_dir.clone());
    let fetch = cached_active_zccache(&paths)?;

    let mut output = CacheFlushOutput {
        schema_version: JSON_SCHEMA_VERSION,
        command: "cache flush",
        cache_dir: active_zccache_dir.display().to_string(),
        flushed: false,
        stats: None,
        notes: Vec::new(),
    };

    // Issue #1286 (F1): also checkpoint the soldr-daemon's EMBEDDED
    // zccache state (the rustc side). The managed-zccache flush below
    // only covers the C/C++ daemon; without this, archives taken after
    // `cache flush` were missing the embedded artifact index + depgraph
    // and restored with zero rustc hits.
    {
        let sock = crate::daemon::server::server_sock_path(&paths);
        match crate::daemon::client::flush_caches(&sock) {
            Ok(()) => output
                .notes
                .push("embedded zccache state flushed via soldr-daemon".into()),
            Err(err) => output.notes.push(format!(
                "soldr-daemon embedded flush unavailable ({err:?}); on-disk \
                 state is whatever the daemon last persisted"
            )),
        }
    }

    let mut lifecycle = linked_private.map(|(lifecycle, _)| lifecycle).or_else(|| {
        fetch
            .as_ref()
            .map(|fetch| ZccacheLifecycle::new(&fetch.binary_path, &zccache_dir))
    });

    if let Some(lifecycle) = lifecycle.as_mut() {
        // soldr#383 contract: ask zccache to fsync its in-memory
        // depgraph (and any other state) to disk and return only once
        // the bytes are durable. Prefer the JSON form so we can
        // re-emit the upstream stats verbatim; fall back to plain
        // `zccache flush` when the build does not yet support
        // `--json`.
        let result = lifecycle.flush()?;
        output.flushed = result.flushed;
        output.stats = result.stats;
        if let Some(snippet) = result.invalid_json {
            output.notes.push(format!(
                "zccache flush --json stdout was not valid JSON: {snippet}"
            ));
        }
        if result.json_unsupported && result.flushed {
            output.notes.push(format!(
                "zccache flush --json not supported by managed zccache {}; ran `zccache flush` instead",
                crate::fetch::MANAGED_ZCCACHE_VERSION
            ));
        }
        if result.subcommand_unsupported {
            output.notes.push(format!(
                "managed zccache {} does not yet implement the `flush` subcommand; upgrade for soldr#383 CI checkpointing",
                crate::fetch::MANAGED_ZCCACHE_VERSION
            ));
        } else if result.already_stopped {
            output.notes.push(
                "daemon was not running; nothing to flush (state on disk is already durable)"
                    .into(),
            );
        } else if let Some(failure) = result.failure {
            let command = if result.json_unsupported {
                "zccache flush"
            } else {
                "zccache flush --json"
            };
            return Err(SoldrError::Other(format!("{command} failed: {failure}")));
        }
    } else {
        output
            .notes
            .push("managed zccache binary not yet fetched; nothing to flush".into());
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
        if let Some(stats) = &output.stats {
            if let Some(bytes) = stats.get("bytes_written").and_then(|v| v.as_u64()) {
                println!("  bytes_written: {bytes}");
            }
            if let Some(secs) = stats.get("duration_ms").and_then(|v| v.as_u64()) {
                println!("  duration_ms: {secs}");
            }
        }
        for note in &output.notes {
            println!("  note: {note}");
        }
    }
    Ok(())
}

pub(super) fn linked_private_zccache_lifecycle(
    paths: &SoldrPaths,
) -> Option<(ZccacheLifecycle, std::path::PathBuf)> {
    let link = db::get_linked_zccache(&db::db_path(paths)).ok().flatten()?;
    if !link.private_daemon {
        return None;
    }
    let daemon_name = link.daemon_name?;
    let binary_path = std::path::PathBuf::from(link.binary_path);
    if !binary_path.exists() {
        return None;
    }
    let cache_dir = std::path::PathBuf::from(link.cache_dir);
    let mut private_daemon = ZccachePrivateDaemonConfig::new(daemon_name);
    private_daemon.owner_pid = link.owner_pid;
    let lifecycle =
        ZccacheLifecycle::new(binary_path, &cache_dir).with_private_daemon(private_daemon);
    Some((lifecycle, cache_dir))
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
