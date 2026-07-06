//! `soldr cache report` — assemble a JSON or human summary of the most
//! recent zccache session, including the optional `zccache analyze` rollup.

use crate::core::{SoldrError, SoldrPaths};
use crate::zccache::managed_zccache_cache_dir;
use crate::JSON_SCHEMA_VERSION;
use serde::Serialize;

use super::{print_json, zccache_output_snippet, EMBEDDED_ZCCACHE_VERSION};

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
    collect_cache_report_output_for_paths(&paths)
}

fn collect_cache_report_output_for_paths(
    paths: &SoldrPaths,
) -> Result<CacheReportOutput, SoldrError> {
    // soldr#1368: no private managed-zccache daemon any more — report on
    // the shared soldr-managed zccache dir.
    let zccache_dir = managed_zccache_cache_dir(paths)?;
    let session_stats_path = crate::cache_lib::session_stats_path(&zccache_dir);
    let journal_path = crate::cache_lib::session_journal_path(&zccache_dir);
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

    // soldr#1368: `zccache analyze` rollups required an externally
    // resolved zccache binary, which no longer exists — rustc compile
    // caching runs through the soldr-daemon embedded service. The
    // per-session `last_session` stats above are still read from disk
    // when present; the analyze rollup is dropped.
    let _ = &zccache_dir;
    let rollups: Option<serde_json::Value> = None;
    if journal_present {
        notes.push(
            "rollups: `zccache analyze` is unavailable — compile caching runs through the              soldr-daemon embedded zccache service (see `soldr daemon status`)"
                .to_string(),
        );
    } else {
        notes
            .push("rollups: journal missing — soldr writes it on cache-enabled builds".to_string());
    }

    Ok(CacheReportOutput {
        schema_version: JSON_SCHEMA_VERSION,
        command: "cache report",
        soldr_version: crate::core::version().to_string(),
        managed_zccache_version: EMBEDDED_ZCCACHE_VERSION,
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

pub(super) fn zccache_analyze_failure_note(
    status_code: Option<i32>,
    stdout: &[u8],
    stderr: &[u8],
) -> String {
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

#[cfg(test)]
mod tests {
    use super::collect_cache_report_output_for_paths;
    use crate::core::SoldrPaths;

    crate::timed_test!(cache_report_reads_session_stats_from_shared_zccache_dir, {
        // soldr#1368: the report reads the last-session stats file from the
        // shared soldr-managed zccache dir (no private-daemon dir any more).
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

        let report = collect_cache_report_output_for_paths(&paths).expect("collect report");
        assert_eq!(report.session_stats_path, stats_path.display().to_string());
        assert!(report.session_stats_present);
        assert_eq!(
            report
                .last_session
                .as_ref()
                .and_then(|v| v.get("hits"))
                .and_then(serde_json::Value::as_u64),
            Some(17)
        );
        assert_eq!(
            report
                .last_session
                .as_ref()
                .and_then(|v| v.get("misses"))
                .and_then(serde_json::Value::as_u64),
            Some(2)
        );
    });
}
