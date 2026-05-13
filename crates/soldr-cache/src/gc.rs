//! `soldr gc` orchestration: scan the target registry, apply safety
//! guards and thresholds, surface candidates, and (optionally) delete
//! their `target/` directories.
//!
//! This module owns the policy. The CLI layer is a thin shim that
//! parses flags, builds a [`GcOptions`], and prints the
//! [`GcReport`] / decides whether to invoke [`GcPlan::apply`].

use crate::target_registry::{
    current_unix_seconds, directory_size, evaluate_safety_guards, human_age, human_size,
    workspace_root_for_target, GuardOutcome, RegistryError, TargetRegistry,
    DEFAULT_STALE_AGE_SECONDS, DEFAULT_STALE_SIZE_BYTES,
};
use std::{
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

const GC_LOG_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone)]
pub struct GcOptions {
    pub older_than_seconds: u64,
    pub larger_than_bytes: u64,
    pub dev_roots: Vec<PathBuf>,
    pub dry_run: bool,
}

impl Default for GcOptions {
    fn default() -> Self {
        Self {
            older_than_seconds: DEFAULT_STALE_AGE_SECONDS,
            larger_than_bytes: DEFAULT_STALE_SIZE_BYTES,
            dev_roots: Vec::new(),
            dry_run: false,
        }
    }
}

/// One concrete candidate `target/` directory after sizing and
/// guard evaluation.
#[derive(Debug, Clone)]
pub struct GcCandidate {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub age_seconds: i64,
    pub eligible: bool,
    pub reason: Option<String>,
}

/// Aggregated scan result for the registry.
#[derive(Debug, Clone, Default)]
pub struct GcReport {
    /// Eligible candidates (over thresholds, guards passed).
    pub candidates: Vec<GcCandidate>,
    /// Skipped entries (guards rejected, under thresholds, or
    /// missing-on-disk rows that were dropped).
    pub skipped: Vec<GcCandidate>,
    /// Number of registry rows whose path didn't exist on disk and
    /// were quietly dropped.
    pub dropped_missing: usize,
}

#[derive(Debug, Clone)]
pub struct GcDeleteOutcome {
    pub candidate: GcCandidate,
    pub removed: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct GcPurgeFailure {
    pub candidate: GcCandidate,
    pub error: String,
}

#[derive(Debug, Clone, Default)]
pub struct GcPurgeSummary {
    pub selected_count: usize,
    pub succeeded_count: usize,
    pub failed_count: usize,
    pub reclaimed_bytes: u64,
    pub deleted_paths: Vec<PathBuf>,
    pub failures: Vec<GcPurgeFailure>,
}

/// Pure scan: walk the registry, drop missing rows, apply thresholds
/// and safety guards. Does not delete anything.
pub fn scan(registry: &TargetRegistry, options: &GcOptions) -> Result<GcReport, RegistryError> {
    let now = current_unix_seconds()?;
    let mut report = GcReport::default();

    for row in registry.list()? {
        if !row.path.exists() {
            // Drop missing-on-disk row silently per the proposal.
            let _ = registry.remove(&row.path);
            report.dropped_missing += 1;
            continue;
        }

        let age = now.saturating_sub(row.last_used);
        let size = directory_size(&row.path);

        if (age as u64) < options.older_than_seconds {
            report.skipped.push(GcCandidate {
                path: row.path,
                size_bytes: size,
                age_seconds: age,
                eligible: false,
                reason: Some(format!(
                    "younger than threshold ({} < {})",
                    human_age(age),
                    human_age(options.older_than_seconds as i64)
                )),
            });
            continue;
        }

        if size < options.larger_than_bytes {
            report.skipped.push(GcCandidate {
                path: row.path,
                size_bytes: size,
                age_seconds: age,
                eligible: false,
                reason: Some(format!(
                    "smaller than threshold ({} < {})",
                    human_size(size),
                    human_size(options.larger_than_bytes)
                )),
            });
            continue;
        }

        let workspace = workspace_root_for_target(&row.path);
        let outcome = evaluate_safety_guards(
            &row.path,
            &workspace,
            &options.dev_roots,
            options.older_than_seconds,
            now,
        );
        match outcome {
            GuardOutcome::Eligible => {
                report.candidates.push(GcCandidate {
                    path: row.path,
                    size_bytes: size,
                    age_seconds: age,
                    eligible: true,
                    reason: None,
                });
            }
            GuardOutcome::Skipped(reason) => {
                report.skipped.push(GcCandidate {
                    path: row.path,
                    size_bytes: size,
                    age_seconds: age,
                    eligible: false,
                    reason: Some(reason),
                });
            }
        }
    }

    Ok(report)
}

/// Delete the `target/` dir at `path` and drop its registry row. Skips
/// the actual delete when `dry_run` is true. Returns whether the
/// directory was removed (`false` when dry-run, when missing, or when
/// the delete failed).
pub fn purge_one(
    registry: &TargetRegistry,
    path: &Path,
    dry_run: bool,
) -> Result<bool, RegistryError> {
    if dry_run {
        return Ok(false);
    }
    if !path.exists() {
        let _ = registry.remove(path);
        return Ok(false);
    }
    let result = std::fs::remove_dir_all(path);
    match result {
        Ok(_) => {
            let _ = registry.remove(path);
            Ok(true)
        }
        Err(e) => Err(RegistryError::Io(e)),
    }
}

pub fn delete_candidate_dir(candidate: GcCandidate) -> GcDeleteOutcome {
    if !candidate.path.exists() {
        return GcDeleteOutcome {
            candidate,
            removed: false,
            error: None,
        };
    }

    match std::fs::remove_dir_all(&candidate.path) {
        Ok(()) => GcDeleteOutcome {
            candidate,
            removed: true,
            error: None,
        },
        Err(e) => GcDeleteOutcome {
            candidate,
            removed: false,
            error: Some(e.to_string()),
        },
    }
}

pub fn apply_purge_outcomes(
    registry: &TargetRegistry,
    outcomes: Vec<GcDeleteOutcome>,
) -> Result<GcPurgeSummary, RegistryError> {
    let mut summary = GcPurgeSummary {
        selected_count: outcomes.len(),
        ..GcPurgeSummary::default()
    };

    for outcome in outcomes {
        if let Some(error) = outcome.error {
            summary.failed_count += 1;
            summary.failures.push(GcPurgeFailure {
                candidate: outcome.candidate,
                error,
            });
            continue;
        }

        let _ = registry.remove(&outcome.candidate.path)?;
        summary.succeeded_count += 1;
        if outcome.removed {
            summary.reclaimed_bytes = summary
                .reclaimed_bytes
                .saturating_add(outcome.candidate.size_bytes);
            summary.deleted_paths.push(outcome.candidate.path);
        }
    }

    Ok(summary)
}

pub fn cleanup_old_gc_logs(log_dir: &Path) -> Result<usize, RegistryError> {
    cleanup_old_gc_logs_with_retention(log_dir, GC_LOG_RETENTION)
}

pub fn cleanup_old_gc_logs_with_retention(
    log_dir: &Path,
    retention: Duration,
) -> Result<usize, RegistryError> {
    let entries = match std::fs::read_dir(log_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    let now = SystemTime::now();
    let mut removed = 0;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if !metadata.is_file() {
            continue;
        }
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        let age = now.duration_since(modified).unwrap_or(Duration::ZERO);
        if age > retention {
            std::fs::remove_file(&path)?;
            removed += 1;
        }
    }
    Ok(removed)
}

pub fn write_gc_error_log(
    log_dir: &Path,
    args: &[String],
    failures: &[GcPurgeFailure],
) -> Result<PathBuf, RegistryError> {
    std::fs::create_dir_all(log_dir)?;
    let now = current_unix_seconds()?;
    let path = log_dir.join(format!("gc-error-{now}-{}.log", std::process::id()));
    let mut file = std::fs::File::create(&path)?;

    writeln!(file, "timestamp_unix={now}")?;
    writeln!(file, "command_args={args:?}")?;
    writeln!(file, "failure_count={}", failures.len())?;
    writeln!(file)?;

    for failure in failures {
        writeln!(file, "path={}", failure.candidate.path.display())?;
        writeln!(file, "size_bytes={}", failure.candidate.size_bytes)?;
        writeln!(file, "age_seconds={}", failure.candidate.age_seconds)?;
        writeln!(file, "error={}", failure.error)?;
        writeln!(file)?;
    }

    Ok(path)
}

/// Information for the once-per-day startup warning. Returns `None`
/// if no candidates qualify or the warning was already emitted today.
pub fn maybe_build_startup_warning(
    registry: &TargetRegistry,
    options: &GcOptions,
    marker_path: &Path,
) -> Result<Option<String>, RegistryError> {
    if !startup_warning_due(marker_path)? {
        return Ok(None);
    }

    let report = scan(registry, options)?;
    if report.candidates.is_empty() {
        return Ok(None);
    }

    let total_bytes: u64 = report.candidates.iter().map(|c| c.size_bytes).sum();
    let n = report.candidates.len();
    let plural = if n == 1 { "" } else { "s" };
    let message = format!(
        "soldr: {n} stale target/ dir{plural} using {} (last used > {}). Run 'soldr gc' to review.",
        human_size(total_bytes),
        human_age(options.older_than_seconds as i64),
    );

    touch_startup_warning_marker(marker_path)?;
    Ok(Some(message))
}

/// Whether enough time has passed since the last warning to emit
/// another one. Missing marker => due now.
pub fn startup_warning_due(marker_path: &Path) -> Result<bool, RegistryError> {
    let metadata = match std::fs::metadata(marker_path) {
        Ok(m) => m,
        Err(_) => return Ok(true),
    };
    let modified = match metadata.modified() {
        Ok(t) => t,
        Err(_) => return Ok(true),
    };
    let now = SystemTime::now();
    let elapsed = now
        .duration_since(modified)
        .unwrap_or(std::time::Duration::ZERO);
    Ok(elapsed.as_secs() >= 24 * 60 * 60)
}

fn touch_startup_warning_marker(path: &Path) -> Result<(), RegistryError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, current_unix_seconds()?.to_string())?;
    Ok(())
}

/// Parse human-friendly duration strings like `10d`, `4h`, `30m`,
/// `90s`. Returns seconds.
pub fn parse_duration(input: &str) -> Result<u64, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("empty duration".to_string());
    }
    let (num_str, unit) = split_numeric_suffix(s);
    let value: u64 = num_str
        .parse()
        .map_err(|_| format!("invalid duration number in {s:?}"))?;
    let multiplier: u64 = match unit.to_ascii_lowercase().as_str() {
        "" | "s" | "sec" | "secs" | "second" | "seconds" => 1,
        "m" | "min" | "mins" | "minute" | "minutes" => 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3_600,
        "d" | "day" | "days" => 86_400,
        "w" | "wk" | "wks" | "week" | "weeks" => 7 * 86_400,
        other => return Err(format!("unknown duration unit {other:?} in {s:?}")),
    };
    value
        .checked_mul(multiplier)
        .ok_or_else(|| format!("duration overflow in {s:?}"))
}

/// Parse human-friendly size strings like `256M`, `1GB`, `512KiB`.
/// Returns bytes. Both decimal (KB, MB) and binary (KiB, MiB)
/// suffixes resolve to powers of 1024.
pub fn parse_size(input: &str) -> Result<u64, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("empty size".to_string());
    }
    let (num_str, unit) = split_numeric_suffix(s);
    let value: u64 = num_str
        .parse()
        .map_err(|_| format!("invalid size number in {s:?}"))?;
    let multiplier: u64 = match unit.to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        "t" | "tb" | "tib" => 1024_u64.pow(4),
        other => return Err(format!("unknown size unit {other:?} in {s:?}")),
    };
    value
        .checked_mul(multiplier)
        .ok_or_else(|| format!("size overflow in {s:?}"))
}

fn split_numeric_suffix(s: &str) -> (&str, &str) {
    let split_at = s.find(|c: char| !(c.is_ascii_digit())).unwrap_or(s.len());
    s.split_at(split_at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target_registry::TargetRegistry;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn make_workspace(root: &Path, name: &str, size_bytes: u64) -> (PathBuf, PathBuf) {
        let workspace = root.join(name);
        let target = workspace.join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("blob"), vec![0u8; size_bytes as usize]).unwrap();
        (workspace, target)
    }

    #[test]
    fn scan_drops_missing_rows_and_keeps_candidates() {
        let dir = tempdir().unwrap();
        let registry = TargetRegistry::open_in_memory().unwrap();
        let (_, target) = make_workspace(dir.path(), "repo-a", 512);
        let missing = dir.path().join("ghost").join("target");

        let now = current_unix_seconds().unwrap();
        registry
            .upsert_with_time(&target, now - 30 * 86_400)
            .unwrap();
        registry
            .upsert_with_time(&missing, now - 30 * 86_400)
            .unwrap();

        let opts = GcOptions {
            older_than_seconds: 10 * 86_400,
            larger_than_bytes: 0, // include everything
            dev_roots: vec![dir.path().to_path_buf()],
            dry_run: true,
        };
        let report = scan(&registry, &opts).unwrap();
        assert_eq!(report.dropped_missing, 1);
        assert_eq!(report.candidates.len(), 1);
        assert_eq!(report.candidates[0].path, target);
    }

    #[test]
    fn scan_filters_by_age_and_size() {
        let dir = tempdir().unwrap();
        let registry = TargetRegistry::open_in_memory().unwrap();
        let (_, target_small) = make_workspace(dir.path(), "small", 16);
        let (_, target_big) = make_workspace(dir.path(), "big", 4096);

        let now = current_unix_seconds().unwrap();
        // Small but stale
        registry
            .upsert_with_time(&target_small, now - 30 * 86_400)
            .unwrap();
        // Big but fresh
        registry.upsert_with_time(&target_big, now - 60).unwrap();

        let opts = GcOptions {
            older_than_seconds: 10 * 86_400,
            larger_than_bytes: 1024,
            dev_roots: vec![dir.path().to_path_buf()],
            dry_run: true,
        };
        let report = scan(&registry, &opts).unwrap();
        assert!(report.candidates.is_empty());
        assert_eq!(report.skipped.len(), 2);
    }

    #[test]
    fn dry_run_purge_does_not_delete() {
        let dir = tempdir().unwrap();
        let registry = TargetRegistry::open_in_memory().unwrap();
        let (_, target) = make_workspace(dir.path(), "repo", 256);
        registry.upsert_with_time(&target, 100).unwrap();

        let removed = purge_one(&registry, &target, true).unwrap();
        assert!(!removed);
        assert!(target.exists(), "dry-run must not delete");
        // Registry row preserved on dry-run.
        assert!(registry.get(&target).unwrap().is_some());
    }

    #[test]
    fn purge_deletes_directory_and_row() {
        let dir = tempdir().unwrap();
        let registry = TargetRegistry::open_in_memory().unwrap();
        let (_, target) = make_workspace(dir.path(), "repo", 256);
        registry.upsert_with_time(&target, 100).unwrap();

        let removed = purge_one(&registry, &target, false).unwrap();
        assert!(removed);
        assert!(!target.exists());
        assert!(registry.get(&target).unwrap().is_none());
    }

    #[test]
    fn purge_outcomes_remove_only_successful_rows() {
        let dir = tempdir().unwrap();
        let registry = TargetRegistry::open_in_memory().unwrap();
        let (_, ok_target) = make_workspace(dir.path(), "ok", 256);
        let (_, failed_target) = make_workspace(dir.path(), "failed", 512);
        registry.upsert_with_time(&ok_target, 100).unwrap();
        registry.upsert_with_time(&failed_target, 100).unwrap();

        let ok_candidate = GcCandidate {
            path: ok_target.clone(),
            size_bytes: 256,
            age_seconds: 1000,
            eligible: true,
            reason: None,
        };
        let failed_candidate = GcCandidate {
            path: failed_target.clone(),
            size_bytes: 512,
            age_seconds: 1000,
            eligible: true,
            reason: None,
        };
        let summary = apply_purge_outcomes(
            &registry,
            vec![
                GcDeleteOutcome {
                    candidate: ok_candidate,
                    removed: true,
                    error: None,
                },
                GcDeleteOutcome {
                    candidate: failed_candidate,
                    removed: false,
                    error: Some("permission denied".to_string()),
                },
            ],
        )
        .unwrap();

        assert_eq!(summary.selected_count, 2);
        assert_eq!(summary.succeeded_count, 1);
        assert_eq!(summary.failed_count, 1);
        assert_eq!(summary.reclaimed_bytes, 256);
        assert!(registry.get(&ok_target).unwrap().is_none());
        assert!(registry.get(&failed_target).unwrap().is_some());
    }

    #[test]
    fn gc_error_log_includes_failure_details() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("repo").join("target");
        let failure = GcPurgeFailure {
            candidate: GcCandidate {
                path: target.clone(),
                size_bytes: 1024,
                age_seconds: 42,
                eligible: true,
                reason: None,
            },
            error: "permission denied".to_string(),
        };

        let log_path = write_gc_error_log(
            dir.path(),
            &["soldr".to_string(), "gc".to_string(), "purge".to_string()],
            &[failure],
        )
        .unwrap();
        let raw = std::fs::read_to_string(log_path).unwrap();

        assert!(raw.contains("command_args="));
        assert!(raw.contains(&target.display().to_string()));
        assert!(raw.contains("size_bytes=1024"));
        assert!(raw.contains("permission denied"));
    }

    #[test]
    fn gc_log_cleanup_removes_entries_past_retention() {
        let dir = tempdir().unwrap();
        let stale = dir.path().join("stale.log");
        let fresh = dir.path().join("fresh.log");
        std::fs::write(&stale, b"old").unwrap();
        std::thread::sleep(Duration::from_millis(30));
        std::fs::write(&fresh, b"new").unwrap();

        let removed = cleanup_old_gc_logs_with_retention(dir.path(), Duration::from_millis(15))
            .expect("cleanup old logs");

        assert_eq!(removed, 1);
        assert!(!stale.exists());
        assert!(fresh.exists());
    }

    #[test]
    fn safety_guard_short_circuits_on_active_lock() {
        let dir = tempdir().unwrap();
        let registry = TargetRegistry::open_in_memory().unwrap();
        let (_, target) = make_workspace(dir.path(), "repo", 4096);
        std::fs::write(target.join(".cargo-lock"), b"").unwrap();

        let now = current_unix_seconds().unwrap();
        registry
            .upsert_with_time(&target, now - 30 * 86_400)
            .unwrap();

        let opts = GcOptions {
            older_than_seconds: 10 * 86_400,
            larger_than_bytes: 0,
            dev_roots: vec![dir.path().to_path_buf()],
            dry_run: true,
        };
        let report = scan(&registry, &opts).unwrap();
        assert!(report.candidates.is_empty());
        assert_eq!(report.skipped.len(), 1);
    }

    #[test]
    fn parse_duration_handles_units() {
        assert_eq!(parse_duration("10d").unwrap(), 10 * 86_400);
        assert_eq!(parse_duration("4h").unwrap(), 4 * 3_600);
        assert_eq!(parse_duration("90s").unwrap(), 90);
        assert_eq!(parse_duration("3W").unwrap(), 3 * 7 * 86_400);
        assert!(parse_duration("garbage").is_err());
    }

    #[test]
    fn parse_size_handles_units() {
        assert_eq!(parse_size("256M").unwrap(), 256 * 1024 * 1024);
        assert_eq!(parse_size("1GB").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_size("512KiB").unwrap(), 512 * 1024);
        assert_eq!(parse_size("42").unwrap(), 42);
        assert!(parse_size("nopes").is_err());
    }

    #[test]
    fn startup_warning_throttle_blocks_repeats() {
        let dir = tempdir().unwrap();
        let marker = dir.path().join(".gc_warning_marker");
        // First call: due.
        assert!(startup_warning_due(&marker).unwrap());
        // After we touch it, next call should not be due.
        touch_startup_warning_marker(&marker).unwrap();
        assert!(!startup_warning_due(&marker).unwrap());
    }
}
