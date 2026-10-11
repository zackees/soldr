//! `soldr logs view` and `soldr logs prune` (soldr#3698).
//!
//! A *launch* is one per-build archive directory named by its decimal
//! session id under the `zccache-build-history` entry that
//! `soldr logs paths` reports. That entry is the only place this module
//! looks: the history root is resolved through
//! [`crate::logs_cmd::build_log_paths_output`], never re-derived, and
//! prune only ever removes direct, non-symlink, all-digit children of it.

use serde::Serialize;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::core::{SoldrError, SoldrPaths};

/// Name of the `soldr logs paths` entry that holds per-launch archives.
const HISTORY_ENTRY: &str = "zccache-build-history";
/// Marker the daemon writes while an archive is still being published.
const PUBLISHING_MARKER: &str = ".publishing-v2";

#[derive(Serialize, Debug, Default)]
pub(crate) struct LogsPruneReport {
    pub schema_version: u32,
    pub history_root: PathBuf,
    pub keep: usize,
    pub dry_run: bool,
    pub kept: Vec<String>,
    pub removed: Vec<String>,
    pub skipped_active: Vec<String>,
    pub failed: Vec<String>,
}

/// The per-launch history root, taken from the `soldr logs paths` inventory.
pub(crate) fn history_root_from_log_paths(paths: &SoldrPaths) -> PathBuf {
    crate::logs_cmd::build_log_paths_output(paths)
        .paths
        .into_iter()
        .find(|entry| entry.name == HISTORY_ENTRY)
        .map(|entry| entry.path)
        .expect("logs paths inventory always names the build-history directory")
}

struct Launch {
    id: String,
    path: PathBuf,
    modified: SystemTime,
}

fn is_launch_id(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit())
}

/// Direct children of `history` that are real directories named by a
/// decimal session id. Symlinks and anything else are ignored.
fn list_launches(history: &Path) -> Result<Vec<Launch>, SoldrError> {
    let entries = match std::fs::read_dir(history) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut launches = Vec::new();
    for entry in entries {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let meta = std::fs::symlink_metadata(entry.path())?;
        if !meta.is_dir() || !is_launch_id(&name) {
            continue;
        }
        launches.push(Launch {
            id: name,
            path: entry.path(),
            modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        });
    }
    // Newest first; the session id breaks ties deterministically.
    launches.sort_by(|a, b| {
        b.modified.cmp(&a.modified).then_with(|| {
            let a_id = a.id.parse::<u128>().unwrap_or(0);
            let b_id = b.id.parse::<u128>().unwrap_or(0);
            b_id.cmp(&a_id)
        })
    });
    Ok(launches)
}

/// Keep the newest `keep` launches under `history` and remove the rest.
/// Launches still being published are never removed.
pub(crate) fn prune_history(
    history: &Path,
    keep: usize,
    dry_run: bool,
) -> Result<LogsPruneReport, SoldrError> {
    let mut report = LogsPruneReport {
        schema_version: 1,
        history_root: history.to_path_buf(),
        keep,
        dry_run,
        ..LogsPruneReport::default()
    };
    for (index, launch) in list_launches(history)?.into_iter().enumerate() {
        if index < keep {
            report.kept.push(launch.id);
            continue;
        }
        if launch.path.join(PUBLISHING_MARKER).exists() {
            report.skipped_active.push(launch.id);
            continue;
        }
        // Defense in depth: only a direct child of the history root.
        if launch.path.parent() != Some(history) {
            report.failed.push(launch.id);
            continue;
        }
        if dry_run {
            report.removed.push(launch.id);
            continue;
        }
        match std::fs::remove_dir_all(&launch.path) {
            Ok(()) => report.removed.push(launch.id),
            Err(error) => {
                eprintln!(
                    "soldr logs prune: failed to remove {}: {error}",
                    launch.path.display()
                );
                report.failed.push(launch.id);
            }
        }
    }
    Ok(report)
}

/// Stream every `*.jsonl` journal of one launch (sorted by file name) to
/// `out`. `launch_id` is an exact id or a unique decimal prefix.
pub(crate) fn view_launch(
    history: &Path,
    launch_id: &str,
    out: &mut dyn Write,
) -> Result<(), SoldrError> {
    let needle = launch_id.trim();
    if !is_launch_id(needle) {
        return Err(SoldrError::Other(format!(
            "launch id must be a decimal session id (see `soldr logs list`): {needle:?}"
        )));
    }
    let launches = list_launches(history)?;
    let exact = launches.iter().find(|l| l.id == needle);
    let matches: Vec<&Launch> = match exact {
        Some(launch) => vec![launch],
        None => launches
            .iter()
            .filter(|l| l.id.starts_with(needle))
            .collect(),
    };
    let launch = match matches.as_slice() {
        [one] => *one,
        [] => {
            return Err(SoldrError::Other(format!(
                "no archived journal for launch {needle} under {}",
                history.display()
            )))
        }
        _ => {
            return Err(SoldrError::Other(format!(
                "launch id prefix is ambiguous: {needle}"
            )))
        }
    };
    let mut journals: Vec<PathBuf> = std::fs::read_dir(&launch.path)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|ext| ext == "jsonl")
                && std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
        })
        .collect();
    journals.sort();
    if journals.is_empty() {
        return Err(SoldrError::Other(format!(
            "launch {} has no JSONL journal in {}",
            launch.id,
            launch.path.display()
        )));
    }
    for journal in journals {
        let mut file = std::fs::File::open(&journal)?;
        std::io::copy(&mut file, out)?;
    }
    out.flush()?;
    Ok(())
}

pub(crate) fn run_logs_view(launch_id: &str) -> Result<i32, SoldrError> {
    let history = history_root_from_log_paths(&SoldrPaths::new()?);
    let stdout = std::io::stdout();
    view_launch(&history, launch_id, &mut stdout.lock())?;
    Ok(0)
}

pub(crate) fn run_logs_prune(keep: usize, dry_run: bool, json: bool) -> Result<i32, SoldrError> {
    let history = history_root_from_log_paths(&SoldrPaths::new()?);
    let report = prune_history(&history, keep, dry_run)?;
    if json {
        let text = serde_json::to_string_pretty(&report)
            .map_err(|error| SoldrError::Other(error.to_string()))?;
        println!("{text}");
    } else {
        let verb = if dry_run { "would remove" } else { "removed" };
        println!(
            "soldr logs prune: {} {} launch(es), kept {}, skipped {} active ({})",
            verb,
            report.removed.len(),
            report.kept.len(),
            report.skipped_active.len(),
            history.display()
        );
    }
    Ok(if report.failed.is_empty() { 0 } else { 1 })
}

#[cfg(test)]
#[path = "logs_retention_tests.rs"]
mod tests;
