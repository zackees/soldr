//! `soldr cache report` — assemble a JSON or human summary of the most
//! recent zccache session, including the optional `zccache analyze` rollup.

use crate::core::{SoldrError, SoldrPaths};
use crate::zccache::{managed_zccache_cache_dir, run_zccache_command_raw_in_cache_dir};
use crate::{cached_active_zccache, JSON_SCHEMA_VERSION};
use serde::Serialize;

use super::{print_json, zccache_output_snippet, zccache_subcommand_unsupported};

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
    let zccache_dir = managed_zccache_cache_dir(&paths)?;
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

    let rollups = if journal_present {
        match cached_active_zccache(&paths)? {
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
                        crate::fetch::MANAGED_ZCCACHE_VERSION
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
        soldr_version: crate::core::version().to_string(),
        managed_zccache_version: crate::fetch::MANAGED_ZCCACHE_VERSION,
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
