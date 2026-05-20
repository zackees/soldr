//! Status, cache inspection, cache prune-target, version, and cache-clearing
//! commands. Extracted from `main.rs` as part of issue #339.

use crate::zccache::{
    managed_zccache_cache_dir, run_zccache_command_in_cache_dir,
    run_zccache_command_raw_in_cache_dir,
};
use crate::{cached_managed_zccache, JSON_SCHEMA_VERSION};
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

pub(crate) fn print_json<T: Serialize>(value: &T) -> Result<(), SoldrError> {
    serde_json::to_writer_pretty(std::io::stdout(), value)
        .map_err(|e| SoldrError::Other(format!("failed to serialize JSON output: {e}")))?;
    println!();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{zccache_analyze_failure_note, zccache_output_snippet, ZCCACHE_ANALYZE_NOTE_LIMIT};

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
}
