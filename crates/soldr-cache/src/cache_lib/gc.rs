//! `soldr gc` orchestration: scan the target registry, apply safety
//! guards and thresholds, surface candidates, and (optionally) delete
//! their `target/` directories.
//!
//! This module owns the policy. The CLI layer is a thin shim that
//! parses flags, builds a [`GcOptions`], and prints the
//! [`GcReport`] / decides whether to invoke [`GcPlan::apply`].

use super::target_registry::{
    current_unix_seconds, directory_size, evaluate_safety_guards, human_age, human_size,
    workspace_root_for_target, GuardOutcome, RegistryError, TargetRegistry, TargetRow,
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

/// Snapshot of the registry taken before any long-running work, so the
/// database handle can be released while that work happens (#1681).
pub struct RegistrySnapshot {
    /// Rows whose directory still exists on disk.
    pub rows: Vec<TargetRow>,
    /// Rows dropped because their path was gone.
    pub dropped_missing: usize,
}

/// Read the registry into an owned snapshot and **release the database
/// handle before returning** (#1681).
///
/// [`TargetRegistry`] holds the process-wide `state_db_open_lock` guard
/// and the redb file lock for its whole lifetime (#608). A GC pass that
/// keeps one alive across directory sizing, per-candidate prompting, and
/// recursive deletion therefore blocks every other `state.redb` opener —
/// `daemon::db`, `cache_lib::cook_index`, and the `RecordTargetTouch`
/// handler that runs on every rustc-wrapper call — for the whole
/// duration. Prompting in particular is unbounded: it waits on a human.
///
/// Pruning missing rows stays inside this short phase because it is the
/// one registry write the scan needs, and it is bounded by the row
/// count rather than by disk size.
pub fn snapshot_registry(db_path: &Path) -> Result<RegistrySnapshot, RegistryError> {
    let registry = TargetRegistry::open(db_path)?;
    let snapshot = snapshot_from_registry(&registry)?;
    drop(registry);
    Ok(snapshot)
}

/// [`snapshot_registry`] against an already-open registry, for callers
/// that own the handle and manage its lifetime themselves.
pub fn snapshot_from_registry(
    registry: &TargetRegistry,
) -> Result<RegistrySnapshot, RegistryError> {
    let mut rows = Vec::new();
    let mut dropped_missing = 0usize;
    for row in registry.list()? {
        if row.path.exists() {
            rows.push(row);
        } else {
            // Drop missing-on-disk row silently per the proposal.
            let _ = registry.remove(&row.path);
            dropped_missing += 1;
        }
    }
    Ok(RegistrySnapshot {
        rows,
        dropped_missing,
    })
}

/// Scan without holding a database handle: open, snapshot, release, then
/// do the sizing and guard evaluation against the owned rows (#1681).
///
/// This is what the `soldr gc` entry points should call. [`scan`] is the
/// handle-holding equivalent, kept for callers that already have a
/// registry open.
pub fn scan_released(db_path: &Path, options: &GcOptions) -> Result<GcReport, RegistryError> {
    let snapshot = snapshot_registry(db_path)?;
    scan_snapshot(snapshot, options)
}

/// Pure scan: walk the registry, drop missing rows, apply thresholds
/// and safety guards. Does not delete anything.
///
/// Holds `registry` for the whole sizing walk. Prefer [`scan_released`]
/// on any path that goes on to prompt or delete (#1681).
pub fn scan(registry: &TargetRegistry, options: &GcOptions) -> Result<GcReport, RegistryError> {
    let snapshot = snapshot_from_registry(registry)?;
    scan_snapshot(snapshot, options)
}

/// How old a tracked `target/` is, taking the *most recent* of two
/// signals (soldr#2134).
///
/// `last_used` is soldr's own bookkeeping: it is stamped when a build goes
/// through the front door. That makes it a poor sole measure of whether a
/// cache is cold, because it goes stale while the directory stays hot:
///
/// * a repo built with bare `cargo` never updates it;
/// * the daemon can lose the touch outright -- `client.rs` reports
///   "target-registry touch was lost: daemon unreachable".
///
/// A target in either state looks like the *oldest* row in the registry
/// (rows are ordered by `last_used` ascending), so it is the first thing
/// eviction reaches for -- which is how a 57.7 GB actively-built cache was
/// purged while a merged worktree's 5.6 GB was kept.
///
/// Consulting the directory's mtime as well can only ever make a target
/// look *younger*, so this is strictly conservative: it can spare a cache
/// from deletion, never cause one. A missing or unreadable mtime falls back
/// to the registry value, which is the previous behaviour.
fn effective_age_seconds(path: &Path, last_used: i64, now: i64) -> i64 {
    let registry_age = now.saturating_sub(last_used);
    let Ok(metadata) = std::fs::metadata(path) else {
        return registry_age;
    };
    let Ok(modified) = metadata.modified() else {
        return registry_age;
    };
    let Ok(since_epoch) = modified.duration_since(SystemTime::UNIX_EPOCH) else {
        return registry_age;
    };
    let fs_age = now.saturating_sub(since_epoch.as_secs() as i64);
    registry_age.min(fs_age)
}

/// Threshold + safety-guard evaluation over an owned snapshot. Touches
/// the filesystem (sizing) but never the database, so it is safe to run
/// with no handle open.
pub fn scan_snapshot(
    snapshot: RegistrySnapshot,
    options: &GcOptions,
) -> Result<GcReport, RegistryError> {
    let now = current_unix_seconds()?;
    let mut report = GcReport {
        dropped_missing: snapshot.dropped_missing,
        ..GcReport::default()
    };

    for row in snapshot.rows {
        let age = effective_age_seconds(&row.path, row.last_used, now);
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
    // Refuse anything that is merely *named* `target` (#1671). Resolution is
    // name-based, so a wrong answer here is a recursive delete of whatever
    // that directory happens to be — an enclosing repository, in the nested
    // case this issue describes. Requiring a cargo marker makes the
    // destructive step prove its subject rather than trust the name.
    if !super::target_registry::looks_like_cargo_target(path) {
        return Err(RegistryError::Io(std::io::Error::other(format!(
            "{} has no cargo target markers; refusing to delete",
            path.display()
        ))));
    }
    let _cargo_locks = match super::cargo_lock::probe(path).map_err(RegistryError::Io)? {
        super::cargo_lock::CargoLockProbe::Idle(guard) => guard,
        super::cargo_lock::CargoLockProbe::Active(lock) => {
            return Err(RegistryError::Io(std::io::Error::other(format!(
                "active cargo lock at {}; refusing to delete",
                lock.display()
            ))));
        }
    };
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

    if !candidate.path.is_dir() {
        return GcDeleteOutcome {
            error: Some(format!(
                "registered target path is not a directory: {}",
                candidate.path.display()
            )),
            candidate,
            removed: false,
        };
    }

    let _cargo_locks = match super::cargo_lock::probe(&candidate.path) {
        Ok(super::cargo_lock::CargoLockProbe::Idle(guard)) => guard,
        Ok(super::cargo_lock::CargoLockProbe::Active(lock)) => {
            return GcDeleteOutcome {
                candidate,
                removed: false,
                error: Some(format!(
                    "active cargo lock at {}; refusing to delete",
                    lock.display()
                )),
            };
        }
        Err(error) => {
            return GcDeleteOutcome {
                candidate,
                removed: false,
                error: Some(format!("cargo lock probe failed closed: {error}")),
            };
        }
    };
    if !super::target_registry::looks_like_cargo_target(&candidate.path) {
        // Same guard as `purge_one` (#1671).
        return GcDeleteOutcome {
            error: Some(format!(
                "{} has no cargo target markers; refusing to delete",
                candidate.path.display()
            )),
            candidate,
            removed: false,
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
    // The marker throttles the expensive registry/filesystem scan, not only
    // visible warnings. Without recording an empty successful scan, machines
    // with no stale candidates repeat the full scan before every build.
    touch_startup_warning_marker(marker_path)?;
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
    use super::super::target_registry::TargetRegistry;
    use super::*;
    use fs2::FileExt;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn make_workspace(root: &Path, name: &str, size_bytes: u64) -> (PathBuf, PathBuf) {
        let workspace = root.join(name);
        let target = workspace.join("target");
        std::fs::create_dir_all(&target).unwrap();
        // Cargo always writes this; the fixture omitted it, which made these
        // targets indistinguishable from any directory called `target`.
        std::fs::write(target.join("CACHEDIR.TAG"), b"Signature: 8a477f597d28d172").unwrap();
        std::fs::write(target.join("blob"), vec![0u8; size_bytes as usize]).unwrap();
        (workspace, target)
    }

    /// #1681: a GC pass must not hold the state-database handle across
    /// its long filesystem/prompting phases.
    ///
    /// `TargetRegistry::open` takes the process-wide `state_db_open_lock`
    /// for the handle's whole lifetime (#608), so anything else that
    /// opens `state.redb` — `daemon::db`, `cook_index`, and the
    /// `RecordTargetTouch` handler on every rustc-wrapper call — is
    /// blocked for as long as GC holds it.
    #[test]
    fn snapshot_releases_the_handle_before_long_work() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("state.redb");
        let (_, target) = make_workspace(dir.path(), "repo-a", 512);
        {
            let registry = TargetRegistry::open(&db).unwrap();
            registry.upsert_with_time(&target, 100).unwrap();
        }

        let snapshot = snapshot_registry(&db).unwrap();
        assert_eq!(snapshot.rows.len(), 1, "the live row must be snapshotted");

        // Stand-in for the long phase: GC owns the rows now and is off
        // sizing, prompting, and deleting. A state write must still get
        // through.
        //
        // Done on another thread with a bounded wait, because `open`
        // blocks rather than failing when the handle is still held:
        // `state_db_open_lock` is a plain in-process mutex, and
        // `open_best_effort`'s short budget covers only the
        // cross-process redb file lock, so it would block here too.
        // Without the thread a regression would hang the whole suite
        // instead of failing; with it, it fails in ten seconds.
        //
        // That same mutex is why there is no negative control: taking a
        // handle and asserting a second open is refused would deadlock
        // the test itself.
        let (tx, rx) = std::sync::mpsc::channel();
        let probe_db = db.clone();
        let probe_target = target.clone();
        std::thread::spawn(move || {
            let _ = tx.send(
                TargetRegistry::open(&probe_db)
                    .and_then(|reg| reg.upsert_with_time(&probe_target, 200)),
            );
        });
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(result) => result.expect("concurrent state write must succeed"),
            Err(_) => panic!(
                "state.redb was still locked while GC did its filesystem work — \
                 the scan is holding its handle across the long phase (#1681)",
            ),
        }

        let opts = GcOptions {
            older_than_seconds: 0,
            larger_than_bytes: 0,
            dev_roots: vec![dir.path().to_path_buf()],
            dry_run: true,
        };
        let report = scan_snapshot(snapshot, &opts).unwrap();
        assert_eq!(
            report.candidates.len() + report.skipped.len(),
            1,
            "the snapshotted row must still be evaluated after the handle was released",
        );
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
        // soldr#2134: cold on disk as well, not just in the registry.
        backdate(&target, 30 * 86_400);

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

    /// Backdate a directory's mtime so the filesystem agrees with a stale
    /// registry stamp. Before soldr#2134 the scan read only the stamp, so
    /// fixtures could leave the real mtime at "just now" without noticing;
    /// now a test that means "this target is cold" has to say so on both
    /// signals.
    fn backdate(path: &Path, seconds_ago: u64) {
        let when = SystemTime::now() - Duration::from_secs(seconds_ago);
        filetime::set_file_mtime(path, filetime::FileTime::from_system_time(when))
            .expect("backdate target mtime");
    }

    #[test]
    fn effective_age_takes_the_more_recent_of_registry_and_mtime() {
        // soldr#2134. The registry stamp is soldr's own bookkeeping and goes
        // stale while a directory stays hot (bare `cargo`, or a lost daemon
        // touch). Whichever signal is more recent wins, so a hot cache cannot
        // be ranked cold by a stale stamp alone.
        let dir = tempdir().unwrap();
        let (_, target) = make_workspace(dir.path(), "repo", 64);
        let now = current_unix_seconds().unwrap();

        // Stamp says 30 days idle; the directory was written just now.
        let age = effective_age_seconds(&target, now - 30 * 86_400, now);
        assert!(
            age < 3600,
            "a freshly written target must not look 30 days old, got {age}s"
        );

        // Both signals old: evaluate 60 days in the future so the filesystem
        // mtime is stale too, without needing to backdate the directory.
        let future = now + 60 * 86_400;
        let age = effective_age_seconds(&target, now - 30 * 86_400, future);
        assert!(
            age >= 59 * 86_400,
            "a genuinely cold target must still read as old, got {age}s"
        );
    }

    #[test]
    fn scan_still_evicts_a_target_that_is_cold_by_both_signals() {
        // The control for the fix: sparing hot caches must not turn into
        // sparing everything. Stale stamp AND stale mtime => still a
        // candidate.
        let dir = tempdir().unwrap();
        let registry = TargetRegistry::open_in_memory().unwrap();
        let (_, target) = make_workspace(dir.path(), "cold", 4096);

        let now = current_unix_seconds().unwrap();
        registry
            .upsert_with_time(&target, now - 30 * 86_400)
            .unwrap();
        backdate(&target, 30 * 86_400);

        let opts = GcOptions {
            older_than_seconds: 10 * 86_400,
            larger_than_bytes: 1024,
            dev_roots: vec![dir.path().to_path_buf()],
            dry_run: true,
        };
        let report = scan(&registry, &opts).unwrap();

        assert_eq!(
            report.candidates.len(),
            1,
            "a target cold by both signals must remain evictable: {report:?}"
        );
    }

    #[test]
    fn scan_spares_a_hot_target_whose_registry_stamp_is_stale() {
        // The reported failure: a 57.7 GB cache written minutes earlier was
        // purged because its registry row was old, while a merged worktree's
        // was kept. Age must not come from the stamp alone.
        let dir = tempdir().unwrap();
        let registry = TargetRegistry::open_in_memory().unwrap();
        let (_, target) = make_workspace(dir.path(), "hot", 4096);

        let now = current_unix_seconds().unwrap();
        registry
            .upsert_with_time(&target, now - 30 * 86_400)
            .unwrap();

        let opts = GcOptions {
            older_than_seconds: 10 * 86_400,
            larger_than_bytes: 1024,
            dev_roots: vec![dir.path().to_path_buf()],
            dry_run: true,
        };
        let report = scan(&registry, &opts).unwrap();

        assert!(
            report.candidates.is_empty(),
            "a target written moments ago must not be an eviction candidate: {:?}",
            report.candidates
        );
        assert_eq!(report.skipped.len(), 1);
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
    fn purge_refuses_a_directory_that_only_looks_like_a_target() {
        // #1671: resolution is name-based, so GC must not delete a directory
        // just because it is called `target`.
        let dir = tempdir().unwrap();
        let registry = TargetRegistry::open_in_memory().unwrap();
        let impostor = dir.path().join("some-repo").join("target");
        std::fs::create_dir_all(&impostor).unwrap();
        std::fs::write(impostor.join("important.txt"), b"not cargo output").unwrap();
        registry.upsert_with_time(&impostor, 100).unwrap();

        let result = purge_one(&registry, &impostor, false);

        assert!(result.is_err(), "must refuse a non-cargo directory");
        assert!(impostor.exists(), "the directory must survive");
        assert!(
            impostor.join("important.txt").exists(),
            "its contents must survive"
        );
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
        // Use explicit mtime manipulation rather than wall-clock sleeps:
        // APFS / NTFS / ext4 all have different mtime resolutions, and a
        // sleep-based version of this test was flaking on macOS CI when
        // the cleanup wall-clock crossed the retention boundary for both
        // files. `FileTimes::set_modified` is millisecond-precise on every
        // supported platform.
        let dir = tempdir().unwrap();
        let stale = dir.path().join("stale.log");
        let fresh = dir.path().join("fresh.log");
        std::fs::write(&stale, b"old").unwrap();
        std::fs::write(&fresh, b"new").unwrap();

        let now = SystemTime::now();
        let stale_mtime = now.checked_sub(Duration::from_secs(60)).unwrap();
        let fresh_mtime = now;
        std::fs::File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_modified(stale_mtime)
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&fresh)
            .unwrap()
            .set_modified(fresh_mtime)
            .unwrap();

        let removed = cleanup_old_gc_logs_with_retention(dir.path(), Duration::from_secs(30))
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
        let cargo_lock = std::fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(target.join(".cargo-lock"))
            .unwrap();
        cargo_lock.try_lock_exclusive().unwrap();

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

    #[test]
    fn empty_startup_scan_is_throttled() {
        let dir = tempdir().unwrap();
        let marker = dir.path().join(".gc_warning_marker");
        let registry = TargetRegistry::open_in_memory().unwrap();
        let opts = GcOptions {
            older_than_seconds: 10 * 86_400,
            larger_than_bytes: 1024,
            dev_roots: vec![dir.path().to_path_buf()],
            dry_run: true,
        };

        assert_eq!(
            maybe_build_startup_warning(&registry, &opts, &marker).unwrap(),
            None
        );
        assert!(marker.is_file(), "successful empty scan must write marker");
        assert!(!startup_warning_due(&marker).unwrap());
    }
}
