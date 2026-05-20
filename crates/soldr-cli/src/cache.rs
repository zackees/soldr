//! Status, cache inspection, cache prune-target, version, and cache-clearing
//! commands. Extracted from `main.rs` as part of issue #339.

use crate::zccache::{
    command_stderr, managed_zccache_cache_dir, run_zccache_command_in_cache_dir,
    run_zccache_command_raw_in_cache_dir, start_zccache_with_recovery,
};
use crate::{cached_managed_zccache, fetch_managed_zccache, JSON_SCHEMA_VERSION};
use serde::Serialize;
use soldr_core::{SoldrError, SoldrPaths};

pub(crate) fn clear_zccache_cache() -> Result<(), SoldrError> {
    let paths = SoldrPaths::new()?;
    let zccache_dir = managed_zccache_cache_dir(&paths)?;
    let mut cleared_anything = false;

    if let Some(fetch) = cached_managed_zccache(&paths)? {
        let _ = run_zccache_command_in_cache_dir(&fetch.binary_path, &["clear"], &zccache_dir)?;
        println!("cleared zccache artifact cache");
        cleared_anything = true;
    }
    if zccache_dir.exists() {
        std::fs::remove_dir_all(&zccache_dir)?;
        println!("removed soldr zccache state dir: {}", zccache_dir.display());
        cleared_anything = true;
    }
    if !cleared_anything {
        println!(
            "managed zccache {} not fetched yet",
            soldr_fetch::MANAGED_ZCCACHE_VERSION
        );
    }
    Ok(())
}

pub(crate) fn purge_soldr_cache() -> Result<(), SoldrError> {
    let paths = SoldrPaths::new()?;
    let mut purged_anything = false;

    purged_anything |= remove_soldr_artifact_dir("cache", &paths.cache)?;
    purged_anything |= remove_soldr_artifact_dir("bin", &paths.bin)?;

    if !purged_anything {
        println!("soldr cache is already empty: {}", paths.root.display());
    }

    Ok(())
}

fn remove_soldr_artifact_dir(label: &str, path: &std::path::Path) -> Result<bool, SoldrError> {
    if !path.exists() {
        return Ok(false);
    }

    if std::fs::symlink_metadata(path)?.file_type().is_dir() {
        std::fs::remove_dir_all(path)?;
        println!("removed soldr {label} dir: {}", path.display());
    } else {
        std::fs::remove_file(path)?;
        println!("removed soldr {label} entry: {}", path.display());
    }
    Ok(true)
}

#[derive(Serialize)]
pub(crate) struct VersionOutput {
    schema_version: u32,
    command: &'static str,
    pub(crate) soldr_version: String,
}

#[derive(Serialize)]
pub(crate) struct StatusOutput {
    schema_version: u32,
    command: &'static str,
    soldr_version: String,
    target: String,
    root_dir: String,
    cache_dir: String,
    cache_default_enabled: bool,
    cache_enabled_for_invocation: bool,
    managed_zccache_version: &'static str,
    zccache: ZccacheStatusSnapshot,
}

#[derive(Serialize)]
pub(crate) struct CacheOutput {
    schema_version: u32,
    command: &'static str,
    soldr_version: String,
    managed_zccache_version: &'static str,
    zccache: ZccacheStatusSnapshot,
}

#[derive(Serialize)]
pub(crate) struct ZccacheStatusSnapshot {
    cache_dir: String,
    state_dir: String,
    session_log_path: String,
    session_log_present: bool,
    journal_path: String,
    journal_present: bool,
    session_stats_path: String,
    session_stats_present: bool,
    binary_path: Option<String>,
    binary_fetched: bool,
    status_lines: Vec<String>,
    status_empty: bool,
}

pub(crate) fn version_output() -> VersionOutput {
    VersionOutput {
        schema_version: JSON_SCHEMA_VERSION,
        command: "version",
        soldr_version: soldr_core::version().to_string(),
    }
}

pub(crate) fn collect_status_output(cache_enabled: bool) -> Result<StatusOutput, SoldrError> {
    let target = soldr_core::TargetTriple::detect()?;
    let paths = SoldrPaths::new()?;
    Ok(StatusOutput {
        schema_version: JSON_SCHEMA_VERSION,
        command: "status",
        soldr_version: soldr_core::version().to_string(),
        target: target.to_string(),
        root_dir: paths.root.display().to_string(),
        cache_dir: paths.cache.display().to_string(),
        cache_default_enabled: true,
        cache_enabled_for_invocation: cache_enabled,
        managed_zccache_version: soldr_fetch::MANAGED_ZCCACHE_VERSION,
        zccache: collect_zccache_status(&paths)?,
    })
}

pub(crate) fn collect_cache_output() -> Result<CacheOutput, SoldrError> {
    let paths = SoldrPaths::new()?;
    Ok(CacheOutput {
        schema_version: JSON_SCHEMA_VERSION,
        command: "cache",
        soldr_version: soldr_core::version().to_string(),
        managed_zccache_version: soldr_fetch::MANAGED_ZCCACHE_VERSION,
        zccache: collect_zccache_status(&paths)?,
    })
}

#[derive(Serialize)]
struct CacheReportOutput {
    schema_version: u32,
    command: &'static str,
    soldr_version: String,
    managed_zccache_version: &'static str,
    /// Path to the per-session stats JSON file.
    session_stats_path: String,
    /// Whether the session-stats file exists on disk.
    session_stats_present: bool,
    /// Path to the per-session JSONL journal.
    journal_path: String,
    /// Whether the journal file exists on disk.
    journal_present: bool,
    /// Verbatim contents of `last-session-stats.json`, parsed into a JSON
    /// value. `null` if the file is missing or unparseable.
    last_session: Option<serde_json::Value>,
    /// Output of `zccache analyze --json` over the per-session journal,
    /// when the managed zccache supports it. `null` otherwise.
    rollups: Option<serde_json::Value>,
    /// Empty for now — populated by future rule passes that turn the
    /// session + rollups into AI-readable diagnoses.
    diagnoses: Vec<serde_json::Value>,
    /// Why a particular field came back null, when relevant. Each entry
    /// is a short string the user can search the soldr docs for.
    notes: Vec<String>,
}

fn collect_cache_report_output() -> Result<CacheReportOutput, SoldrError> {
    let paths = SoldrPaths::new()?;
    let zccache_dir = managed_zccache_cache_dir(&paths)?;
    let session_stats_path = soldr_cache::session_stats_path(&zccache_dir);
    let journal_path = soldr_cache::session_journal_path(&zccache_dir);
    let session_stats_present = session_stats_path.exists();
    let journal_present = journal_path.exists();

    let mut notes: Vec<String> = Vec::new();

    let last_session = if session_stats_present {
        match std::fs::read_to_string(&session_stats_path) {
            Ok(s) => match serde_json::from_str::<serde_json::Value>(s.trim()) {
                Ok(v) => Some(v),
                Err(e) => {
                    notes.push(format!("last_session: unparseable JSON ({e})"));
                    None
                }
            },
            Err(e) => {
                notes.push(format!("last_session: read failed ({e})"));
                None
            }
        }
    } else {
        notes.push(
            "last_session: file missing — run a build with managed zccache first".to_string(),
        );
        None
    };

    let rollups = if journal_present {
        match cached_managed_zccache(&paths)? {
            Some(fetch) => {
                let journal_arg = journal_path.display().to_string();
                let result = run_zccache_command_raw_in_cache_dir(
                    &fetch.binary_path,
                    &["analyze", &journal_arg, "--json"],
                    &zccache_dir,
                )?;
                if result.status.success() {
                    let stdout = String::from_utf8_lossy(&result.stdout);
                    match serde_json::from_str::<serde_json::Value>(stdout.trim()) {
                        Ok(v) => Some(v),
                        Err(e) => {
                            notes
                                .push(format!("rollups: zccache analyze stdout unparseable ({e})"));
                            None
                        }
                    }
                } else if zccache_subcommand_unsupported(&result, "analyze") {
                    notes.push(format!(
                        "rollups: managed zccache {} does not yet support `analyze` — upgrade to 1.5.0+",
                        soldr_fetch::MANAGED_ZCCACHE_VERSION
                    ));
                    None
                } else {
                    notes.push(zccache_analyze_failure_note(
                        result.status.code(),
                        &result.stdout,
                        &result.stderr,
                    ));
                    None
                }
            }
            None => {
                notes.push(
                    "rollups: managed zccache binary not yet fetched (no builds run yet)"
                        .to_string(),
                );
                None
            }
        }
    } else {
        notes
            .push("rollups: journal missing — soldr writes it on cache-enabled builds".to_string());
        None
    };

    Ok(CacheReportOutput {
        schema_version: JSON_SCHEMA_VERSION,
        command: "cache report",
        soldr_version: soldr_core::version().to_string(),
        managed_zccache_version: soldr_fetch::MANAGED_ZCCACHE_VERSION,
        session_stats_path: session_stats_path.display().to_string(),
        session_stats_present,
        journal_path: journal_path.display().to_string(),
        journal_present,
        last_session,
        rollups,
        diagnoses: Vec::new(),
        notes,
    })
}

/// Heuristic: detect whether a `zccache <subcommand>` invocation failed
/// because the subcommand does not exist in the running binary (clap
/// emits "error: unrecognized subcommand"). Used to differentiate
/// version-skew misses from real failures.
fn zccache_subcommand_unsupported(output: &std::process::Output, subcommand: &str) -> bool {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let needles = [
        "unrecognized subcommand",
        "unrecognized command",
        "error: subcommand",
        "invalid value for",
    ];
    let combined = format!("{stderr}\n{stdout}");
    needles.iter().any(|n| combined.contains(n)) && combined.contains(subcommand)
}

const ZCCACHE_ANALYZE_NOTE_LIMIT: usize = 1000;

fn zccache_analyze_failure_note(status_code: Option<i32>, stdout: &[u8], stderr: &[u8]) -> String {
    let mut note = format!("rollups: zccache analyze exited with status {status_code:?}");
    if let Some(stderr) = zccache_output_snippet(stderr) {
        note.push_str("; stderr: ");
        note.push_str(&stderr);
    }
    if let Some(stdout) = zccache_output_snippet(stdout) {
        note.push_str("; stdout: ");
        note.push_str(&stdout);
    }
    note
}

fn zccache_output_snippet(output: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(output);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut compact = String::new();
    let mut previous_was_space = false;
    for ch in trimmed.chars() {
        if ch.is_whitespace() {
            if !previous_was_space {
                compact.push(' ');
                previous_was_space = true;
            }
        } else {
            compact.push(ch);
            previous_was_space = false;
        }
    }

    let mut chars = compact.chars();
    let mut snippet: String = chars.by_ref().take(ZCCACHE_ANALYZE_NOTE_LIMIT).collect();
    if chars.next().is_some() {
        snippet.push_str("...");
    }
    Some(snippet)
}

pub(crate) fn run_cache_prune_target_command(
    target_dir: std::path::PathBuf,
    dry_run: bool,
    json: bool,
) -> Result<(), SoldrError> {
    let canonical = std::path::absolute(&target_dir).unwrap_or_else(|_| target_dir.clone());
    let opts = soldr_cache::prune_target::PruneTargetOptions {
        target_dir: canonical.clone(),
        dry_run,
    };
    let report = soldr_cache::prune_target::prune_target(&opts)
        .map_err(|e| SoldrError::Other(format!("cache prune-target failed: {e}")))?;

    if json {
        let output = build_cache_prune_target_output(&canonical, dry_run, &report);
        print_json(&output)?;
    } else {
        print_cache_prune_target_text(&canonical, dry_run, &report);
    }
    Ok(())
}

fn build_cache_prune_target_output(
    target_dir: &std::path::Path,
    dry_run: bool,
    report: &soldr_cache::prune_target::PruneTargetReport,
) -> CachePruneTargetOutput {
    CachePruneTargetOutput {
        schema_version: JSON_SCHEMA_VERSION,
        command: "cache prune-target",
        target_dir: target_dir.display().to_string(),
        dry_run,
        scanned: report.scanned,
        kept: report.kept,
        deleted: report.deleted,
        reclaimed_bytes: report.reclaimed_bytes,
        reclaimed_human: soldr_cache::target_registry::human_size(report.reclaimed_bytes),
        entries: report
            .entries
            .iter()
            .map(|entry| CachePruneTargetEntryOutput {
                path: entry.path.display().to_string(),
                prefix: entry.prefix.clone(),
                hash: entry.hash.clone(),
                size_bytes: entry.size_bytes,
                size_human: soldr_cache::target_registry::human_size(entry.size_bytes),
                mtime_unix: entry.mtime_unix,
                action: match entry.action {
                    soldr_cache::prune_target::PruneAction::Keep => "keep",
                    soldr_cache::prune_target::PruneAction::Delete => "delete",
                },
            })
            .collect(),
    }
}

fn print_cache_prune_target_text(
    target_dir: &std::path::Path,
    dry_run: bool,
    report: &soldr_cache::prune_target::PruneTargetReport,
) {
    println!("soldr cache prune-target: {}", target_dir.display());
    println!(
        "  mode: {}",
        if dry_run {
            "dry-run (use --force to actually delete)"
        } else {
            "force"
        }
    );
    println!(
        "  scanned={} kept={} deleted={} reclaimed={}",
        report.scanned,
        report.kept,
        report.deleted,
        soldr_cache::target_registry::human_size(report.reclaimed_bytes),
    );
    let mut shown = 0usize;
    for entry in &report.entries {
        if entry.action != soldr_cache::prune_target::PruneAction::Delete {
            continue;
        }
        if shown == 0 {
            println!(
                "  {} entries:",
                if dry_run { "would delete" } else { "deleted" }
            );
        }
        println!(
            "    - {} ({})",
            entry.path.display(),
            soldr_cache::target_registry::human_size(entry.size_bytes),
        );
        shown += 1;
    }
    if shown == 0 {
        println!("  nothing to prune");
    }
}

#[derive(Serialize)]
struct CachePruneTargetOutput {
    schema_version: u32,
    command: &'static str,
    target_dir: String,
    dry_run: bool,
    scanned: usize,
    kept: usize,
    deleted: usize,
    reclaimed_bytes: u64,
    reclaimed_human: String,
    entries: Vec<CachePruneTargetEntryOutput>,
}

#[derive(Serialize)]
struct CachePruneTargetEntryOutput {
    path: String,
    prefix: String,
    hash: String,
    size_bytes: u64,
    size_human: String,
    mtime_unix: i64,
    action: &'static str,
}

pub(crate) fn run_cache_report_command(json: bool) -> Result<(), SoldrError> {
    let output = collect_cache_report_output()?;
    if json {
        print_json(&output)?;
    } else {
        print_cache_report_output(&output);
    }
    Ok(())
}

fn print_cache_report_output(output: &CacheReportOutput) {
    println!("soldr cache report");
    println!(
        "  session-stats: {} ({})",
        output.session_stats_path,
        if output.session_stats_present {
            "present"
        } else {
            "missing"
        }
    );
    println!(
        "  journal:       {} ({})",
        output.journal_path,
        if output.journal_present {
            "present"
        } else {
            "missing"
        }
    );
    if let Some(stats) = &output.last_session {
        if let Some(rate) = stats.get("hit_rate").and_then(|v| v.as_f64()) {
            println!("  hit_rate:      {:.1}%", rate * 100.0);
        }
        if let Some(hits) = stats.get("hits").and_then(|v| v.as_u64()) {
            let misses = stats.get("misses").and_then(|v| v.as_u64()).unwrap_or(0);
            println!("  hits/misses:   {hits}/{misses}");
        }
        if let Some(saved_ms) = stats.get("time_saved_ms").and_then(|v| v.as_u64()) {
            println!("  time_saved:    {saved_ms} ms");
        }
    }
    if let Some(rollups) = &output.rollups {
        if let Some(by_ext) = rollups.get("by_extension").and_then(|v| v.as_object()) {
            if !by_ext.is_empty() {
                println!("  by extension:");
                for (ext, bucket) in by_ext {
                    let h = bucket.get("hits").and_then(|v| v.as_u64()).unwrap_or(0);
                    let m = bucket.get("misses").and_then(|v| v.as_u64()).unwrap_or(0);
                    println!("    {ext:<14}  hits={h}  misses={m}");
                }
            }
        }
    }
    if !output.notes.is_empty() {
        println!("  notes:");
        for note in &output.notes {
            println!("    - {note}");
        }
    }
}

fn collect_zccache_status(paths: &SoldrPaths) -> Result<ZccacheStatusSnapshot, SoldrError> {
    let zccache_dir = managed_zccache_cache_dir(paths)?;
    let session_log_path = soldr_cache::session_log_path(&zccache_dir);
    let session_log_present = session_log_path.exists();
    let journal_path = soldr_cache::session_journal_path(&zccache_dir);
    let journal_present = journal_path.exists();
    let session_stats_path = soldr_cache::session_stats_path(&zccache_dir);
    let session_stats_present = session_stats_path.exists();

    match cached_managed_zccache(paths)? {
        Some(fetch) => {
            let output =
                run_zccache_command_in_cache_dir(&fetch.binary_path, &["status"], &zccache_dir)?;
            let stdout = output.stdout.trim();
            let status_lines = stdout.lines().map(str::to_owned).collect();
            Ok(ZccacheStatusSnapshot {
                cache_dir: zccache_dir.display().to_string(),
                state_dir: zccache_dir.display().to_string(),
                session_log_path: session_log_path.display().to_string(),
                session_log_present,
                journal_path: journal_path.display().to_string(),
                journal_present,
                session_stats_path: session_stats_path.display().to_string(),
                session_stats_present,
                binary_path: Some(fetch.binary_path.display().to_string()),
                binary_fetched: true,
                status_lines,
                status_empty: stdout.is_empty(),
            })
        }
        None => Ok(ZccacheStatusSnapshot {
            cache_dir: zccache_dir.display().to_string(),
            state_dir: zccache_dir.display().to_string(),
            session_log_path: session_log_path.display().to_string(),
            session_log_present,
            journal_path: journal_path.display().to_string(),
            journal_present,
            session_stats_path: session_stats_path.display().to_string(),
            session_stats_present,
            binary_path: None,
            binary_fetched: false,
            status_lines: Vec::new(),
            status_empty: false,
        }),
    }
}

pub(crate) fn print_status_output(output: &StatusOutput) {
    println!("soldr {}", output.soldr_version);
    println!("target: {}", output.target);
    println!("root dir: {}", output.root_dir);
    println!("cache dir: {}", output.cache_dir);
    println!("cache default: enabled");
    println!(
        "cache mode: {}",
        if output.cache_enabled_for_invocation {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!("zccache version: {}", output.managed_zccache_version);
    print_zccache_status_snapshot(&output.zccache);
}

pub(crate) fn print_cache_output(output: &CacheOutput) {
    print_zccache_status_snapshot(&output.zccache);
}

fn print_zccache_status_snapshot(snapshot: &ZccacheStatusSnapshot) {
    println!("soldr zccache cache dir: {}", snapshot.cache_dir);
    println!("soldr zccache state dir: {}", snapshot.state_dir);
    println!(
        "last session log: {} ({})",
        snapshot.session_log_path,
        if snapshot.session_log_present {
            "present"
        } else {
            "missing"
        }
    );
    println!(
        "last session journal: {} ({})",
        snapshot.journal_path,
        if snapshot.journal_present {
            "present"
        } else {
            "missing"
        }
    );
    println!(
        "last session stats: {} ({})",
        snapshot.session_stats_path,
        if snapshot.session_stats_present {
            "present"
        } else {
            "missing"
        }
    );

    if let Some(binary_path) = &snapshot.binary_path {
        println!("zccache binary: {binary_path}");
        if snapshot.status_empty {
            println!("zccache status: no output");
        } else {
            for line in &snapshot.status_lines {
                println!("zccache: {line}");
            }
        }
    } else {
        println!(
            "zccache binary: not fetched yet (will fetch managed zccache {} on the first cache-enabled build)",
            soldr_fetch::MANAGED_ZCCACHE_VERSION
        );
    }
}

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

    let session_log_path = log.unwrap_or_else(|| soldr_cache::session_log_path(&zccache_dir));
    let journal_path = journal.unwrap_or_else(|| soldr_cache::session_journal_path(&zccache_dir));
    let session_stats_path = soldr_cache::session_stats_path(&zccache_dir);

    // Idempotent path: if ZCCACHE_SESSION_ID is already set and the
    // caller did not pass an explicit --id, reuse the existing session
    // without contacting the daemon. soldr#379 requires this so
    // setup-soldr can call session-start repeatedly without spawning
    // orphan sessions.
    if id.is_none() {
        if let Ok(existing) = std::env::var(soldr_cache::ZCCACHE_SESSION_ID_ENV_VAR) {
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

    let fetch = fetch_managed_zccache(&paths).await?;
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
        soldr_cache::parse_zccache_session_id(&session_json.stdout).ok_or_else(|| {
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
        std::env::var(soldr_cache::ZCCACHE_SESSION_ID_ENV_VAR)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    }) {
        Some(value) => value,
        None => {
            return Err(SoldrError::Other(format!(
                "session-end requires --id or ${} to be set",
                soldr_cache::ZCCACHE_SESSION_ID_ENV_VAR
            )));
        }
    };

    let paths = SoldrPaths::new()?;
    let zccache_dir = managed_zccache_cache_dir(&paths)?;
    let fetch = cached_managed_zccache(&paths)?.ok_or_else(|| {
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
        soldr_cache::session_journal_path(zccache_dir),
        soldr_cache::session_log_path(zccache_dir),
        soldr_cache::session_stats_path(zccache_dir),
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

    let fetch = cached_managed_zccache(&paths)?;
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

    let env_session_id = std::env::var(soldr_cache::ZCCACHE_SESSION_ID_ENV_VAR)
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
            soldr_cache::session_log_path(&zccache_dir),
            soldr_cache::session_journal_path(&zccache_dir),
            soldr_cache::session_stats_path(&zccache_dir),
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
    let fetch = cached_managed_zccache(&paths)?;

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
                    soldr_fetch::MANAGED_ZCCACHE_VERSION
                ));
            } else if zccache_subcommand_unsupported(&retry, "flush") {
                output.notes.push(format!(
                    "managed zccache {} does not yet implement the `flush` subcommand; upgrade for soldr#383 CI checkpointing",
                    soldr_fetch::MANAGED_ZCCACHE_VERSION
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
                soldr_fetch::MANAGED_ZCCACHE_VERSION
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

fn zccache_daemon_already_stopped(output: &std::process::Output) -> bool {
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    )
    .to_ascii_lowercase();
    combined.contains("daemon not running")
        || combined.contains("no daemon to stop")
        || combined.contains("not running")
        || combined.contains("connection refused")
}

pub(crate) fn print_json<T: Serialize>(value: &T) -> Result<(), SoldrError> {
    serde_json::to_writer_pretty(std::io::stdout(), value)
        .map_err(|e| SoldrError::Other(format!("failed to serialize JSON output: {e}")))?;
    println!();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        clear_session_artifacts, zccache_analyze_failure_note, zccache_daemon_already_stopped,
        zccache_flag_unsupported, zccache_output_snippet, zccache_session_already_ended,
        ZCCACHE_ANALYZE_NOTE_LIMIT,
    };
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
        let log = soldr_cache::session_log_path(zccache_dir);
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
