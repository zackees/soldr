//! `soldr cache report` — assemble a JSON or human summary of the most
//! recent zccache session, including the optional `zccache analyze` rollup.

use crate::core::{SoldrError, SoldrPaths};
use crate::zccache::managed_zccache_cache_dir;
use crate::JSON_SCHEMA_VERSION;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use super::{output_snippet, print_json, EMBEDDED_ZCCACHE_VERSION};

#[derive(Serialize)]
pub(super) struct CacheReportOutput {
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
    /// Build session selected for this report, when build history is available.
    session_id: Option<String>,
    /// Workspace from which the report was requested.
    workspace_root: String,
    /// Unix timestamp (milliseconds) of the selected stats file, when available.
    stats_written_at: Option<i64>,
    /// Whether this report selected immutable workspace history or the legacy
    /// global last-writer-wins files.
    session_source: &'static str,
    /// Verbatim contents of `last-session-stats.json`, parsed into a JSON
    /// value. `null` if the file is missing or unparseable. Kept as a raw
    /// `Value` (not a typed struct) on purpose: zccache evolves its
    /// `SessionStats` shape across protocol versions, and downstream
    /// consumers (perf-rust-cluster, `ci/perf_local.py`) rely on new
    /// fields like `phase_profile` (zccache PROTOCOL_VERSION 9) reaching
    /// them without a soldr release. See soldr#430 — the
    /// `cache_report_json_passes_through_unknown_session_stat_fields`
    /// integration test locks this contract in.
    last_session: Option<serde_json::Value>,
    /// Output of `zccache analyze --json` over the per-session journal,
    /// when an analyzer surface is available. `null` otherwise.
    rollups: Option<serde_json::Value>,
    /// Empty for now — populated by future rule passes that turn the
    /// session + rollups into AI-readable diagnoses.
    diagnoses: Vec<serde_json::Value>,
    /// Why a particular field came back null, when relevant. Each entry
    /// is a short string the user can search the soldr docs for.
    notes: Vec<String>,
}

struct SelectedSession {
    stats_path: PathBuf,
    journal_path: PathBuf,
    session_id: Option<String>,
    source: &'static str,
}

fn collect_cache_report_output() -> Result<CacheReportOutput, SoldrError> {
    let paths = SoldrPaths::new()?;
    let workspace_root = std::env::current_dir().map_err(SoldrError::from)?;
    collect_cache_report_output_for_workspace(&paths, &workspace_root)
}

fn collect_cache_report_output_for_workspace(
    paths: &SoldrPaths,
    workspace_root: &Path,
) -> Result<CacheReportOutput, SoldrError> {
    collect_cache_report_output_with_history(paths, workspace_root, None)
}

fn collect_cache_report_output_with_history(
    paths: &SoldrPaths,
    workspace_root: &Path,
    build_history: Option<Vec<crate::daemon::protocol::BuildRecord>>,
) -> Result<CacheReportOutput, SoldrError> {
    // soldr#1368: no private standalone zccache daemon any more — report on
    // the shared Soldr-owned embedded-service directory.
    let zccache_dir = managed_zccache_cache_dir(paths)?;
    let global_stats_path = crate::cache_lib::session_stats_path(&zccache_dir);
    let global_journal_path = crate::cache_lib::session_journal_path(&zccache_dir);
    let mut notes: Vec<String> = Vec::new();
    let selected = match build_history {
        Some(records) => select_workspace_session_from_records(
            records,
            workspace_root,
            global_stats_path,
            global_journal_path,
            &mut notes,
        ),
        None => select_workspace_session(
            paths,
            workspace_root,
            global_stats_path,
            global_journal_path,
            &mut notes,
        ),
    };
    let session_stats_path = selected.stats_path;
    let journal_path = selected.journal_path;
    let session_stats_present = session_stats_path.exists();
    let journal_present = journal_path.exists();

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
        notes
            .push("last_session: file missing — run a cache-enabled Soldr build first".to_string());
        None
    };

    // soldr#1368: `zccache analyze` rollups required an externally
    // resolved zccache binary, which no longer exists — rustc compile
    // caching runs through the soldr-daemon embedded service. The
    // per-session `last_session` stats above are still read from disk
    // when present; the analyze rollup is dropped.
    let _ = &zccache_dir;
    let rollups: Option<serde_json::Value> = None;
    if journal_present {
        notes.push(
            "rollups: `zccache analyze` is unavailable — compile caching runs through the soldr-daemon embedded zccache service (see `soldr daemon status`)"
                .to_string(),
        );
    } else {
        notes
            .push("rollups: journal missing — soldr writes it on cache-enabled builds".to_string());
    }

    let mut diagnoses = Vec::new();
    add_publication_diagnosis(&last_session, &mut diagnoses);

    Ok(CacheReportOutput {
        schema_version: JSON_SCHEMA_VERSION,
        command: "cache report",
        soldr_version: crate::core::version().to_string(),
        managed_zccache_version: EMBEDDED_ZCCACHE_VERSION,
        session_stats_path: session_stats_path.display().to_string(),
        session_stats_present,
        journal_path: journal_path.display().to_string(),
        journal_present,
        session_id: selected.session_id,
        workspace_root: workspace_root.display().to_string(),
        stats_written_at: file_modified_unix_ms(&session_stats_path),
        session_source: selected.source,
        last_session,
        rollups,
        diagnoses,
        notes,
    })
}

fn select_workspace_session(
    paths: &SoldrPaths,
    workspace_root: &Path,
    global_stats_path: PathBuf,
    global_journal_path: PathBuf,
    notes: &mut Vec<String>,
) -> SelectedSession {
    let sock = crate::daemon::client::default_sock_path(paths);
    match crate::daemon::client::list_builds(&sock, 10_000, None) {
        Ok(records) => {
            return select_workspace_session_from_records(
                records,
                workspace_root,
                global_stats_path,
                global_journal_path,
                notes,
            );
        }
        Err(error) => notes.push(format!(
                "provenance: could not read build history ({error:?}); using global last-writer-wins stats {} (originating workspace: unknown)",
            global_stats_path.display(),
        )),
    }
    SelectedSession {
        stats_path: global_stats_path,
        journal_path: global_journal_path,
        session_id: None,
        source: "global-fallback",
    }
}

fn select_workspace_session_from_records(
    records: Vec<crate::daemon::protocol::BuildRecord>,
    workspace_root: &Path,
    global_stats_path: PathBuf,
    global_journal_path: PathBuf,
    notes: &mut Vec<String>,
) -> SelectedSession {
    for record in records {
        if !same_workspace_path(Path::new(&record.repo_root), workspace_root) {
            continue;
        }
        let Some(log_paths) = record.log_paths else {
            continue;
        };
        let Some(stats_path) = log_paths.archived_session_stats_path.map(PathBuf::from) else {
            continue;
        };
        if !stats_path.is_file() {
            continue;
        }
        let journal_path = log_paths
            .archived_journal_path
            .map(PathBuf::from)
            .unwrap_or_else(|| stats_path.with_file_name("last-session.jsonl"));
        notes.push(format!(
            "provenance: workspace history session {} ({})",
            record.session_id,
            stats_path.display()
        ));
        return SelectedSession {
            stats_path,
            journal_path,
            session_id: Some(record.session_id.to_string()),
            source: "workspace-history",
        };
    }
    notes.push(format!(
        "provenance: no archived build history matches workspace {}; using global last-writer-wins stats {} (originating workspace: unknown)",
        workspace_root.display(),
        global_stats_path.display(),
    ));
    SelectedSession {
        stats_path: global_stats_path,
        journal_path: global_journal_path,
        session_id: None,
        source: "global-fallback",
    }
}

fn same_workspace_path(left: &Path, right: &Path) -> bool {
    let left = std::fs::canonicalize(left).unwrap_or_else(|_| left.to_path_buf());
    let right = std::fs::canonicalize(right).unwrap_or_else(|_| right.to_path_buf());
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    } else {
        left == right
    }
}

fn file_modified_unix_ms(path: &Path) -> Option<i64> {
    let duration = std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?;
    i64::try_from(duration.as_millis()).ok()
}

fn add_publication_diagnosis(
    last_session: &Option<serde_json::Value>,
    diagnoses: &mut Vec<serde_json::Value>,
) {
    let Some(stats) = last_session else {
        return;
    };
    let misses = stats.get("misses").and_then(serde_json::Value::as_u64);
    let publication_success = stats
        .pointer("/phase_profile/staged/counters/publication_success")
        .and_then(serde_json::Value::as_u64);
    if matches!((misses, publication_success), (Some(misses), Some(0)) if misses > 0) {
        diagnoses.push(serde_json::json!({
            "kind": "cache_publication_failed",
            "severity": "warning",
            "message": "cacheable compilations succeeded but none became durable; the cache will not warm",
        }));
    }
}

pub(super) fn zccache_analyze_failure_note(
    status_code: Option<i32>,
    stdout: &[u8],
    stderr: &[u8],
) -> String {
    let mut note = format!("rollups: zccache analyze exited with status {status_code:?}");
    if let Some(stderr) = output_snippet(stderr) {
        note.push_str("; stderr: ");
        note.push_str(&stderr);
    }
    if let Some(stdout) = output_snippet(stdout) {
        note.push_str("; stdout: ");
        note.push_str(&stdout);
    }
    note
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
    println!("  workspace:     {}", output.workspace_root);
    println!(
        "  session:       {} ({})",
        output.session_id.as_deref().unwrap_or("unknown"),
        output.session_source
    );
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
    for diagnosis in &output.diagnoses {
        if diagnosis
            .get("severity")
            .and_then(serde_json::Value::as_str)
            == Some("warning")
        {
            if let Some(message) = diagnosis.get("message").and_then(serde_json::Value::as_str) {
                println!("  WARNING: {message}");
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

#[cfg(test)]
mod tests {
    use super::collect_cache_report_output_with_history;
    use crate::core::SoldrPaths;
    use crate::daemon::protocol::{BuildLogPaths, BuildRecord};

    fn build_record(session_id: u64, repo_root: String, stats_path: String) -> BuildRecord {
        BuildRecord {
            session_id,
            repo_root,
            started_at_ms: session_id as i64,
            ended_at_ms: Some(session_id as i64 + 1),
            exit_code: Some(0),
            total_wall_ms: Some(1),
            crate_count: 0,
            slowest_crate_us: None,
            slowest_crate_name: None,
            cache_summary: None,
            log_paths: Some(BuildLogPaths {
                zccache_session_id: Some(session_id.to_string()),
                cache_dir: None,
                session_log_path: None,
                journal_path: None,
                session_stats_path: None,
                compile_journal_path: None,
                archived_session_log_path: None,
                archived_journal_path: None,
                archived_session_stats_path: Some(stats_path),
                archived_compile_journal_path: None,
                private_daemon_name: None,
            }),
            miss_reasons: Vec::new(),
        }
    }

    #[test]
    fn cache_report_reads_session_stats_from_shared_zccache_dir() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("soldr"));
        let zccache_dir = crate::zccache::managed_zccache_cache_dir(&paths).expect("zccache dir");
        let stats_path = crate::cache_lib::session_stats_path(&zccache_dir);
        std::fs::create_dir_all(stats_path.parent().expect("stats parent"))
            .expect("create logs dir");
        std::fs::write(
            &stats_path,
            r#"{"status":"ok","session_id":"shared","hits":17,"misses":2,"hit_rate":0.89}"#,
        )
        .expect("write stats");

        let workspace_root = std::env::current_dir().expect("current dir");
        let archived_stats_path = paths
            .cache
            .join("zccache/history/42/last-session-stats.json");
        std::fs::create_dir_all(archived_stats_path.parent().expect("archive parent"))
            .expect("create archive dir");
        std::fs::write(
            &archived_stats_path,
            r#"{"status":"ok","session_id":"42","hits":23,"misses":5,"hit_rate":0.82}"#,
        )
        .expect("write archived stats");
        let record = build_record(
            42,
            workspace_root.display().to_string(),
            archived_stats_path.display().to_string(),
        );
        let report =
            collect_cache_report_output_with_history(&paths, &workspace_root, Some(vec![record]))
                .expect("collect report");
        assert_eq!(
            report.session_stats_path,
            archived_stats_path.display().to_string(),
            "the active workspace must use its archived build session, not the global last-writer-wins file",
        );
        assert!(report.session_stats_present);
        assert_eq!(
            report
                .last_session
                .as_ref()
                .and_then(|v| v.get("hits"))
                .and_then(serde_json::Value::as_u64),
            Some(23)
        );
        assert_eq!(
            report
                .last_session
                .as_ref()
                .and_then(|v| v.get("misses"))
                .and_then(serde_json::Value::as_u64),
            Some(5)
        );
        assert_eq!(report.session_id.as_deref(), Some("42"));
        assert_eq!(report.session_source, "workspace-history");
        assert_eq!(
            report.workspace_root,
            workspace_root.display().to_string(),
            "report provenance names the workspace that selected this archived session"
        );
        let json = serde_json::to_value(&report).expect("serialize report");
        assert_eq!(json["session_id"], "42");
        assert_eq!(json["workspace_root"], workspace_root.display().to_string());
        assert!(
            json["stats_written_at"].as_i64().is_some(),
            "JSON report carries the archived stats file timestamp: {json:#?}"
        );
    }

    #[test]
    fn cache_report_warns_when_cacheable_misses_do_not_publish() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("soldr"));
        let workspace_root = temp.path().join("workspace");
        std::fs::create_dir_all(&workspace_root).expect("create workspace");
        let archived_stats_path = paths
            .cache
            .join("zccache/history/99/last-session-stats.json");
        std::fs::create_dir_all(archived_stats_path.parent().expect("archive parent"))
            .expect("create archive dir");
        std::fs::write(
            &archived_stats_path,
            r#"{
                "status":"ok",
                "hits":0,
                "misses":7,
                "phase_profile":{"staged":{"counters":{"publication_success":0}}}
            }"#,
        )
        .expect("write archived stats");
        let record = build_record(
            99,
            workspace_root.display().to_string(),
            archived_stats_path.display().to_string(),
        );
        let report =
            collect_cache_report_output_with_history(&paths, &workspace_root, Some(vec![record]))
                .expect("collect report");
        assert!(report.diagnoses.iter().any(|diagnosis| {
            diagnosis.get("kind").and_then(serde_json::Value::as_str)
                == Some("cache_publication_failed")
        }));
    }
}
