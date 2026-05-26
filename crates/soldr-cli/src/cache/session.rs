//! `soldr session start/end`, `soldr cache shutdown`, and `soldr cache flush` —
//! lifecycle commands for the zccache daemon and its per-session journal.

use crate::core::{SoldrError, SoldrPaths};
use crate::zccache::{
    command_stderr, managed_zccache_cache_dir, run_zccache_command_in_cache_dir,
    run_zccache_command_raw_in_cache_dir, start_zccache_with_recovery,
};
use crate::{cached_active_zccache, fetch_active_zccache, JSON_SCHEMA_VERSION};
use serde::Serialize;

use super::{
    zccache_daemon_already_stopped, zccache_output_snippet, zccache_subcommand_unsupported,
};

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
    start_zccache_with_recovery(&fetch.binary_path, &zccache_dir)?;

    let log_arg = session_log_path.display().to_string();
    let journal_arg = journal_path.display().to_string();
    let mut args: Vec<&str> = vec![
        "session-start",
        "--stats",
        "--log",
        &log_arg,
        "--journal",
        &journal_arg,
    ];
    if let Some(id_value) = id.as_deref() {
        args.push("--id");
        args.push(id_value);
    }
    let session_json = run_zccache_command_in_cache_dir(&fetch.binary_path, &args, &zccache_dir)?;
    let session_id =
        crate::cache_lib::parse_zccache_session_id(&session_json.stdout).ok_or_else(|| {
            SoldrError::Other(format!(
                "failed to parse zccache session id from output: {}",
                session_json.stdout.trim()
            ))
        })?;

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

    let result = run_zccache_command_raw_in_cache_dir(
        &fetch.binary_path,
        &["session-end", &session_id, "--json"],
        &zccache_dir,
    )?;
    let (stats, already_ended) = if result.status.success() {
        let stdout = String::from_utf8_lossy(&result.stdout);
        let parsed = if stdout.trim().is_empty() {
            None
        } else {
            serde_json::from_str::<serde_json::Value>(stdout.trim()).ok()
        };
        (parsed, false)
    } else if zccache_session_already_ended(&result) {
        (None, true)
    } else {
        return Err(SoldrError::Other(format!(
            "zccache session-end {} --json failed: {}",
            session_id,
            command_stderr(&result)
        )));
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

fn zccache_session_already_ended(output: &std::process::Output) -> bool {
    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    stderr.contains("session not found")
        || stderr.contains("no such session")
        || stderr.contains("already ended")
        || stderr.contains("unknown session")
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
    let mut notes: Vec<String> = Vec::new();

    let fetch = cached_active_zccache(&paths)?;
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

    // Step 1: end the active session so zccache flushes the per-session
    // journal before the daemon stops.
    if let (Some(session_id), Some(fetch)) = (env_session_id.as_deref(), fetch.as_ref()) {
        let end = run_zccache_command_raw_in_cache_dir(
            &fetch.binary_path,
            &["session-end", session_id, "--json"],
            &zccache_dir,
        )?;
        if end.status.success() || zccache_session_already_ended(&end) {
            output.session_id = Some(session_id.to_string());
        } else {
            notes.push(format!(
                "session-end {session_id} failed: {}",
                command_stderr(&end)
            ));
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
            crate::cache_lib::session_log_path(&zccache_dir),
            crate::cache_lib::session_journal_path(&zccache_dir),
            crate::cache_lib::session_stats_path(&zccache_dir),
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
    if let Some(fetch) = fetch.as_ref() {
        let mut args: Vec<&str> = vec!["stop"];
        if no_depgraph_save {
            // The flag exists upstream as `--no-depgraph-cache` on
            // `zccache start`. Forward the equivalent suppressor on
            // stop when zccache exposes it; if not, surface a note so
            // operators know it was a no-op.
            args.push("--no-depgraph-save");
        }
        let stop_result =
            run_zccache_command_raw_in_cache_dir(&fetch.binary_path, &args, &zccache_dir)?;
        if stop_result.status.success() {
            output.daemon_stopped = true;
        } else if no_depgraph_save && zccache_flag_unsupported(&stop_result, "--no-depgraph-save") {
            notes.push(
                "zccache stop does not support --no-depgraph-save; retrying with default flush"
                    .into(),
            );
            let retry =
                run_zccache_command_raw_in_cache_dir(&fetch.binary_path, &["stop"], &zccache_dir)?;
            if retry.status.success() {
                output.daemon_stopped = true;
            } else if !zccache_daemon_already_stopped(&retry) {
                notes.push(format!("zccache stop failed: {}", command_stderr(&retry)));
            }
        } else if zccache_daemon_already_stopped(&stop_result) {
            notes.push("daemon was already stopped".into());
        } else {
            notes.push(format!(
                "zccache stop failed: {}",
                command_stderr(&stop_result)
            ));
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
        if let Some(fetch) = fetch.as_ref() {
            polled_for_exit = true;
            match poll_zccache_daemon_exit(
                &fetch.binary_path,
                &zccache_dir,
                std::time::Duration::from_secs(shutdown_timeout_seconds),
            ) {
                DaemonExitPollResult::Exited => {
                    output.daemon_exited = true;
                }
                DaemonExitPollResult::TimedOut => {
                    notes.push(format!(
                        "daemon did not exit within {shutdown_timeout_seconds}s after `zccache stop`; depgraph state may not be durable on disk"
                    ));
                }
                DaemonExitPollResult::PollFailed(err) => {
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

/// Result of polling `zccache status` after a `zccache stop`. Used by
/// `run_cache_shutdown_command` to determine whether the daemon's
/// depgraph snapshot is durable on disk.
enum DaemonExitPollResult {
    /// `zccache status` reported the daemon is no longer running.
    Exited,
    /// Deadline elapsed before the daemon stopped responding.
    TimedOut,
    /// `zccache status` itself failed in an unexpected way (e.g. the
    /// binary became unreadable). The shutdown is treated as
    /// indeterminate.
    PollFailed(String),
}

fn poll_zccache_daemon_exit(
    binary: &std::path::Path,
    zccache_dir: &std::path::Path,
    timeout: std::time::Duration,
) -> DaemonExitPollResult {
    let deadline = std::time::Instant::now() + timeout;
    let poll_interval = std::time::Duration::from_millis(100);
    loop {
        match run_zccache_command_raw_in_cache_dir(binary, &["status"], zccache_dir) {
            Ok(output) => {
                // If `zccache status` errored out with a
                // daemon-not-running phrase, the daemon is gone and
                // the on-disk state from `stop` is durable.
                if !output.status.success() && zccache_daemon_already_stopped(&output) {
                    return DaemonExitPollResult::Exited;
                }
                // Some zccache builds may print the daemon-stopped
                // marker on stdout while still exiting 0 (e.g. a
                // future "status --json" with state="stopped"). Cover
                // that path too without committing to a JSON schema.
                let combined = format!(
                    "{}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                )
                .to_ascii_lowercase();
                if combined.contains("daemon not running")
                    || combined.contains("no daemon")
                    || combined.contains("connection refused")
                {
                    return DaemonExitPollResult::Exited;
                }
            }
            Err(err) => {
                // Spawning zccache itself failed — surface the cause,
                // do not retry.
                return DaemonExitPollResult::PollFailed(err.to_string());
            }
        }
        if std::time::Instant::now() >= deadline {
            return DaemonExitPollResult::TimedOut;
        }
        std::thread::sleep(poll_interval);
    }
}

pub(crate) async fn run_cache_flush_command(json: bool) -> Result<(), SoldrError> {
    let paths = SoldrPaths::new()?;
    let zccache_dir = managed_zccache_cache_dir(&paths)?;
    let fetch = cached_active_zccache(&paths)?;

    let mut output = CacheFlushOutput {
        schema_version: JSON_SCHEMA_VERSION,
        command: "cache flush",
        cache_dir: zccache_dir.display().to_string(),
        flushed: false,
        stats: None,
        notes: Vec::new(),
    };

    if let Some(fetch) = fetch.as_ref() {
        // soldr#383 contract: ask zccache to fsync its in-memory
        // depgraph (and any other state) to disk and return only once
        // the bytes are durable. Prefer the JSON form so we can
        // re-emit the upstream stats verbatim; fall back to plain
        // `zccache flush` when the build does not yet support
        // `--json`.
        let result = run_zccache_command_raw_in_cache_dir(
            &fetch.binary_path,
            &["flush", "--json"],
            &zccache_dir,
        )?;
        if result.status.success() {
            output.flushed = true;
            let stdout = String::from_utf8_lossy(&result.stdout);
            let trimmed = stdout.trim();
            if !trimmed.is_empty() {
                output.stats = serde_json::from_str(trimmed).ok();
                if output.stats.is_none() {
                    output.notes.push(format!(
                        "zccache flush --json stdout was not valid JSON: {}",
                        zccache_output_snippet(trimmed.as_bytes())
                            .unwrap_or_else(|| "<empty>".into())
                    ));
                }
            }
        } else if zccache_flag_unsupported(&result, "--json") {
            // zccache does not yet implement `flush --json`. Retry the
            // bare form so we still get a durable on-disk snapshot.
            let retry =
                run_zccache_command_raw_in_cache_dir(&fetch.binary_path, &["flush"], &zccache_dir)?;
            if retry.status.success() {
                output.flushed = true;
                output.notes.push(format!(
                    "zccache flush --json not supported by managed zccache {}; ran `zccache flush` instead",
                    crate::fetch::MANAGED_ZCCACHE_VERSION
                ));
            } else if zccache_subcommand_unsupported(&retry, "flush") {
                output.notes.push(format!(
                    "managed zccache {} does not yet implement the `flush` subcommand; upgrade for soldr#383 CI checkpointing",
                    crate::fetch::MANAGED_ZCCACHE_VERSION
                ));
            } else if zccache_daemon_already_stopped(&retry) {
                output.notes.push(
                    "daemon was not running; nothing to flush (state on disk is already durable)"
                        .into(),
                );
            } else {
                return Err(SoldrError::Other(format!(
                    "zccache flush failed: {}",
                    command_stderr(&retry)
                )));
            }
        } else if zccache_subcommand_unsupported(&result, "flush") {
            output.notes.push(format!(
                "managed zccache {} does not yet implement the `flush` subcommand; upgrade for soldr#383 CI checkpointing",
                crate::fetch::MANAGED_ZCCACHE_VERSION
            ));
        } else if zccache_daemon_already_stopped(&result) {
            output.notes.push(
                "daemon was not running; nothing to flush (state on disk is already durable)"
                    .into(),
            );
        } else {
            return Err(SoldrError::Other(format!(
                "zccache flush --json failed: {}",
                command_stderr(&result)
            )));
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

fn zccache_flag_unsupported(output: &std::process::Output, flag: &str) -> bool {
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    combined.contains("unexpected argument") && combined.contains(flag.trim_start_matches('-'))
        || combined.contains(&format!("unknown flag: {flag}"))
        || combined.contains(&format!("unrecognized option {flag}"))
}

#[cfg(test)]
mod tests {
    use super::super::report::zccache_analyze_failure_note;
    use super::super::{
        zccache_daemon_already_stopped, zccache_output_snippet, ZCCACHE_ANALYZE_NOTE_LIMIT,
    };
    use super::{clear_session_artifacts, zccache_flag_unsupported, zccache_session_already_ended};
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
        use super::{poll_zccache_daemon_exit, DaemonExitPollResult};
        use std::time::Duration;

        let tmp = tempfile::tempdir().expect("tempdir");
        let bogus = tmp.path().join("definitely-not-zccache");
        let result = poll_zccache_daemon_exit(&bogus, tmp.path(), Duration::from_millis(50));
        assert!(
            matches!(result, DaemonExitPollResult::PollFailed(_)),
            "expected PollFailed when zccache binary cannot be spawned"
        );
    }
}
