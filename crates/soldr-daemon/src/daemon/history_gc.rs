//! Bounded, root-local build-history retention (#1763).

use crate::cache_lib::target_registry::directory_size;
use crate::core::SoldrPaths;
use crate::daemon::db;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(4 * 24 * 60 * 60);
pub const DEFAULT_MAX_BYTES: u64 = 1024 * 1024 * 1024;
const SANITIZED_MIGRATION_MARKER: &str = ".sanitized-history-v1";
const LEGACY_SESSION_FILES_MIGRATION_MARKER: &str = ".legacy-session-files-v1";
const COMPLETE_MARKER: &str = ".complete-v2";
const PUBLISHING_MARKER: &str = ".publishing-v2";
const ABANDONED_PUBLISHING_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const LEGACY_SESSION_FILE_NAMES: [&str; 2] = ["last-session.log", "last-session.jsonl"];

#[derive(Debug, Clone)]
pub struct HistoryGcOptions {
    pub now: SystemTime,
    pub max_age: Duration,
    pub max_bytes: u64,
    /// Run one-time completed-history migrations. This removes every
    /// pre-redaction archive after zccache#1149 and strips the stale fixed
    /// `last-session` files introduced by the embedded-service transition.
    /// Markers are persisted inside the owning root.
    pub migrate_pre_redaction: bool,
}

impl Default for HistoryGcOptions {
    fn default() -> Self {
        Self {
            now: SystemTime::now(),
            max_age: DEFAULT_MAX_AGE,
            max_bytes: DEFAULT_MAX_BYTES,
            migrate_pre_redaction: true,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistoryGcReport {
    pub scanned: usize,
    pub protected_active: usize,
    pub age_removed: usize,
    pub size_removed: usize,
    pub migration_removed: usize,
    pub legacy_files_removed: usize,
    pub failed: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub bytes_reclaimed: u64,
    pub legacy_bytes_reclaimed: u64,
    pub database_rows_updated: u64,
}

#[derive(Debug)]
struct Entry {
    session_id: u64,
    path: PathBuf,
    bytes: u64,
    completed_at: SystemTime,
    active: bool,
}

pub fn history_root(paths: &SoldrPaths) -> PathBuf {
    paths.cache.join("zccache").join("history")
}

pub fn mark_history_complete(archive_dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(archive_dir)?;
    std::fs::write(archive_dir.join(COMPLETE_MARKER), b"sanitized-v1\n")?;
    match std::fs::remove_file(archive_dir.join(PUBLISHING_MARKER)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub fn mark_history_publishing(archive_dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(archive_dir)?;
    std::fs::write(archive_dir.join(PUBLISHING_MARKER), b"publishing\n")
}

/// Every file this module writes to manage an archive's lifecycle. Anything
/// else in the directory is payload.
const MARKER_FILES: [&str; 4] = [
    COMPLETE_MARKER,
    PUBLISHING_MARKER,
    SANITIZED_MIGRATION_MARKER,
    LEGACY_SESSION_FILES_MIGRATION_MARKER,
];

/// Publishes a finished archive, or discards it when it carries no payload.
/// Returns whether it was published.
///
/// The copy helpers that fill an archive silently no-op when their source
/// file is missing, so a build can reach publication with nothing copied.
/// Marking that complete publishes a session that *claims* completeness and
/// contains no data, and every consumer that walks history — `soldr cache`
/// reporting, workspace-history provenance, the retention pass below — then
/// treats it as a real session.
///
/// Deleting is the right disposal rather than simply withholding the marker:
/// `sweep` reads a marker-less session as still-active and will never reclaim
/// it, so skipping the marker would leak the directory forever. (soldr#2186)
pub fn publish_or_discard(archive_dir: &Path) -> std::io::Result<bool> {
    if archive_has_payload(archive_dir)? {
        mark_history_complete(archive_dir)?;
        return Ok(true);
    }
    match std::fs::remove_dir_all(archive_dir) {
        Ok(()) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// True when the archive holds at least one file that is not one of this
/// module's own markers. A missing directory has no payload.
fn archive_has_payload(archive_dir: &Path) -> std::io::Result<bool> {
    let entries = match std::fs::read_dir(archive_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let name = entry?.file_name();
        if !MARKER_FILES.contains(&name.to_string_lossy().as_ref()) {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn sweep(paths: &SoldrPaths, db_path: &Path, options: &HistoryGcOptions) -> HistoryGcReport {
    sweep_with_ops(
        paths,
        db_path,
        options,
        directory_size,
        |path| db::list_builds(path, u32::MAX, None).map_err(|error| error.to_string()),
        |path, ids| db::mark_archives_unavailable(path, ids).map_err(|error| error.to_string()),
    )
}

fn sweep_with_size<F>(
    paths: &SoldrPaths,
    db_path: &Path,
    options: &HistoryGcOptions,
    mut size_of: F,
) -> HistoryGcReport
where
    F: FnMut(&Path) -> u64,
{
    sweep_with_ops(
        paths,
        db_path,
        options,
        &mut size_of,
        |path| db::list_builds(path, u32::MAX, None).map_err(|error| error.to_string()),
        |path, ids| db::mark_archives_unavailable(path, ids).map_err(|error| error.to_string()),
    )
}

fn sweep_with_ops<F, L, M>(
    paths: &SoldrPaths,
    db_path: &Path,
    options: &HistoryGcOptions,
    mut size_of: F,
    mut list_builds: L,
    mut mark_unavailable: M,
) -> HistoryGcReport
where
    F: FnMut(&Path) -> u64,
    L: FnMut(&Path) -> Result<Vec<crate::daemon::protocol::BuildRecord>, String>,
    M: FnMut(&Path, &[u64]) -> Result<u64, String>,
{
    sweep_with_ops_and_remove(
        paths,
        db_path,
        options,
        &mut size_of,
        &mut list_builds,
        &mut mark_unavailable,
        |path| std::fs::remove_dir_all(path),
    )
}

fn sweep_with_ops_and_remove<F, L, M, D>(
    paths: &SoldrPaths,
    db_path: &Path,
    options: &HistoryGcOptions,
    mut size_of: F,
    mut list_builds: L,
    mut mark_unavailable: M,
    mut remove_dir: D,
) -> HistoryGcReport
where
    F: FnMut(&Path) -> u64,
    L: FnMut(&Path) -> Result<Vec<crate::daemon::protocol::BuildRecord>, String>,
    M: FnMut(&Path, &[u64]) -> Result<u64, String>,
    D: FnMut(&Path) -> std::io::Result<()>,
{
    let root = history_root(paths);
    let mut report = HistoryGcReport::default();
    let records = match list_builds(db_path) {
        Ok(records) => records,
        Err(_) => {
            report.failed = 1;
            return report;
        }
    };
    let mut records: HashMap<_, _> = records
        .into_iter()
        .map(|record| (record.session_id, record))
        .collect();
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return report;
        }
        Err(_) => {
            report.failed = 1;
            return report;
        }
    };
    if crate::cache_lib::path_safety::validate_owned_directory(&paths.root, &root).is_err() {
        report.failed = 1;
        return report;
    }
    let mut candidates = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            report.failed += 1;
            continue;
        };
        let Ok(kind) = std::fs::symlink_metadata(entry.path()) else {
            report.failed += 1;
            continue;
        };
        if crate::cache_lib::path_safety::is_link_or_reparse(&kind) {
            report.failed += 1;
            continue;
        }
        if !kind.is_dir() {
            continue;
        }
        let Some(session_id) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        else {
            continue;
        };
        let path = entry.path();
        let record = records.get(&session_id);
        let complete = path.join(COMPLETE_MARKER).is_file();
        let publishing_is_recent = if complete {
            false
        } else {
            match std::fs::symlink_metadata(path.join(PUBLISHING_MARKER)) {
                Ok(metadata) => match metadata.modified() {
                    Ok(modified) => {
                        options.now.duration_since(modified).unwrap_or_default()
                            < ABANDONED_PUBLISHING_MAX_AGE
                    }
                    Err(_) => {
                        report.failed += 1;
                        continue;
                    }
                },
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(_) => {
                    report.failed += 1;
                    continue;
                }
            }
        };
        // Build liveness is governed by the root-wide OS lease before this
        // sweep starts. An unfinished database row is not a liveness signal:
        // a killed client may never send BuildSessionEnd. Only an in-progress
        // publisher marker (or an unknown, incomplete directory) protects the
        // on-disk archive here.
        let active = publishing_is_recent || (record.is_none() && !complete);
        let completed_at = record
            .and_then(|record| record.ended_at_ms)
            .and_then(system_time_from_millis)
            .or_else(|| {
                std::fs::symlink_metadata(&path)
                    .and_then(|meta| meta.modified())
                    .ok()
            });
        let Some(completed_at) = completed_at else {
            report.failed += 1;
            continue;
        };
        let bytes = size_of(&path);
        report.bytes_before = report.bytes_before.saturating_add(bytes);
        candidates.push(Entry {
            session_id,
            path,
            bytes,
            completed_at,
            active,
        });
    }
    report.scanned = candidates.len();
    report.protected_active = candidates.iter().filter(|entry| entry.active).count();

    let migration_due =
        options.migrate_pre_redaction && !root.join(SANITIZED_MIGRATION_MARKER).is_file();
    let legacy_migration_due = options.migrate_pre_redaction
        && !root.join(LEGACY_SESSION_FILES_MIGRATION_MARKER).is_file();
    let mut legacy_migration_pending = false;
    if legacy_migration_due {
        let mut rows_to_clear = Vec::new();
        for entry in &mut candidates {
            let has_legacy_paths = records
                .get(&entry.session_id)
                .and_then(|record| record.log_paths.as_ref())
                .is_some_and(|paths| {
                    paths.session_log_path.is_some()
                        || paths.journal_path.is_some()
                        || paths.archived_session_log_path.is_some()
                        || paths.archived_journal_path.is_some()
                });
            let complete = entry.path.join(COMPLETE_MARKER).is_file();
            if entry.active || !complete {
                match contains_legacy_session_files(&entry.path) {
                    Ok(has_files) => {
                        legacy_migration_pending |= has_files || has_legacy_paths;
                    }
                    Err(_) => {
                        report.failed += 1;
                        legacy_migration_pending = true;
                    }
                }
                continue;
            }

            let mut entry_failed = false;
            for file_name in LEGACY_SESSION_FILE_NAMES {
                match remove_legacy_session_file(&paths.root, &entry.path, file_name) {
                    Ok(Some(bytes)) => {
                        report.legacy_files_removed += 1;
                        report.legacy_bytes_reclaimed =
                            report.legacy_bytes_reclaimed.saturating_add(bytes);
                        report.bytes_reclaimed = report.bytes_reclaimed.saturating_add(bytes);
                        entry.bytes = entry.bytes.saturating_sub(bytes);
                    }
                    Ok(None) => {}
                    Err(_) => {
                        report.failed += 1;
                        entry_failed = true;
                    }
                }
            }
            if entry_failed {
                legacy_migration_pending = true;
            } else if has_legacy_paths {
                rows_to_clear.push(entry.session_id);
            }
        }
        if !rows_to_clear.is_empty() {
            match db::clear_legacy_archive_paths(db_path, &rows_to_clear) {
                Ok(updated) => {
                    report.database_rows_updated =
                        report.database_rows_updated.saturating_add(updated);
                    for session_id in &rows_to_clear {
                        let Some(paths) = records
                            .get_mut(session_id)
                            .and_then(|record| record.log_paths.as_mut())
                        else {
                            continue;
                        };
                        paths.session_log_path = None;
                        paths.journal_path = None;
                        paths.archived_session_log_path = None;
                        paths.archived_journal_path = None;
                    }
                }
                Err(_) => {
                    report.failed += 1;
                    legacy_migration_pending = true;
                }
            }
        }
    }
    candidates.sort_by(|left, right| {
        left.completed_at
            .cmp(&right.completed_at)
            .then_with(|| left.session_id.cmp(&right.session_id))
    });

    let mut selected = HashMap::<u64, &'static str>::new();
    for entry in candidates.iter().filter(|entry| !entry.active) {
        if migration_due && !entry.path.join(COMPLETE_MARKER).is_file() {
            selected.insert(entry.session_id, "migration");
            continue;
        }
        let age = options
            .now
            .duration_since(entry.completed_at)
            .unwrap_or_default();
        if age >= options.max_age {
            selected.insert(entry.session_id, "age");
        }
    }

    let mut bytes_after_plan = report
        .bytes_before
        .saturating_sub(report.legacy_bytes_reclaimed)
        .saturating_sub(
            candidates
                .iter()
                .filter(|entry| selected.contains_key(&entry.session_id))
                .map(|entry| entry.bytes)
                .sum::<u64>(),
        );
    for entry in &candidates {
        if entry.active || selected.contains_key(&entry.session_id) {
            continue;
        }
        if bytes_after_plan <= options.max_bytes {
            break;
        }
        selected.insert(entry.session_id, "size");
        bytes_after_plan = bytes_after_plan.saturating_sub(entry.bytes);
    }

    let mut removed_ids = Vec::new();
    for entry in &candidates {
        let Some(reason) = selected.get(&entry.session_id) else {
            continue;
        };
        let updated = match mark_unavailable(db_path, &[entry.session_id]) {
            Ok(updated) => {
                report.database_rows_updated = report.database_rows_updated.saturating_add(updated);
                updated
            }
            Err(_) => {
                report.failed += 1;
                continue;
            }
        };
        match remove_dir(&entry.path) {
            Ok(()) => {
                removed_ids.push(entry.session_id);
                report.bytes_reclaimed = report.bytes_reclaimed.saturating_add(entry.bytes);
                match *reason {
                    "migration" => report.migration_removed += 1,
                    "age" => report.age_removed += 1,
                    _ => report.size_removed += 1,
                }
            }
            Err(_) => {
                report.failed += 1;
                // A transient unlink failure must not hide a still-present
                // archive. Restore the original record so history readers can
                // continue using it and a later GC pass can retry.
                if let Some(record) = records.get(&entry.session_id) {
                    if db::upsert_build(db_path, record).is_ok() {
                        report.database_rows_updated =
                            report.database_rows_updated.saturating_sub(updated);
                    } else {
                        report.failed += 1;
                    }
                }
            }
        }
    }
    report.bytes_after = report.bytes_before.saturating_sub(report.bytes_reclaimed);
    let migration_pending = candidates.iter().any(|entry| {
        !entry.path.join(COMPLETE_MARKER).is_file() && !removed_ids.contains(&entry.session_id)
    });
    if migration_due
        && report.failed == 0
        && !migration_pending
        && std::fs::create_dir_all(&root)
            .and_then(|()| std::fs::write(root.join(SANITIZED_MIGRATION_MARKER), b"complete\n"))
            .is_err()
    {
        report.failed += 1;
    }
    if legacy_migration_due
        && report.failed == 0
        && !legacy_migration_pending
        && std::fs::create_dir_all(&root)
            .and_then(|()| {
                std::fs::write(
                    root.join(LEGACY_SESSION_FILES_MIGRATION_MARKER),
                    b"complete\n",
                )
            })
            .is_err()
    {
        report.failed += 1;
    }
    report
}

fn contains_legacy_session_files(archive_dir: &Path) -> std::io::Result<bool> {
    for file_name in LEGACY_SESSION_FILE_NAMES {
        match std::fs::symlink_metadata(archive_dir.join(file_name)) {
            Ok(_) => return Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

fn remove_legacy_session_file(
    owned_root: &Path,
    archive_dir: &Path,
    file_name: &str,
) -> std::io::Result<Option<u64>> {
    crate::cache_lib::path_safety::validate_owned_directory(owned_root, archive_dir)?;
    let path = archive_dir.join(file_name);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if crate::cache_lib::path_safety::is_link_or_reparse(&metadata) || !metadata.is_file() {
        return Err(std::io::Error::other(format!(
            "refusing non-regular legacy history artifact {}",
            path.display()
        )));
    }
    let bytes = metadata.len();
    // Revalidate immediately before the destructive operation. The archive
    // was safe when enumerated, but another process may have replaced its
    // numeric directory with a symlink/junction in the meantime.
    crate::cache_lib::path_safety::validate_owned_directory(owned_root, archive_dir)?;
    std::fs::remove_file(&path)?;
    Ok(Some(bytes))
}

fn system_time_from_millis(value: i64) -> Option<SystemTime> {
    u64::try_from(value)
        .ok()
        .map(|millis| UNIX_EPOCH + Duration::from_millis(millis))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::protocol::{BuildLogPaths, BuildRecord};

    fn record(id: u64, ended_at_ms: Option<i64>, archive: &Path) -> BuildRecord {
        BuildRecord {
            session_id: id,
            repo_root: "/repo".into(),
            started_at_ms: 1,
            ended_at_ms,
            exit_code: ended_at_ms.map(|_| 0),
            total_wall_ms: Some(1),
            crate_count: 1,
            slowest_crate_us: None,
            slowest_crate_name: None,
            cache_summary: None,
            log_paths: Some(BuildLogPaths {
                zccache_session_id: None,
                cache_dir: None,
                session_log_path: None,
                journal_path: None,
                session_stats_path: None,
                compile_journal_path: None,
                archived_session_log_path: Some(archive.join("log").display().to_string()),
                archived_journal_path: Some(archive.join("journal").display().to_string()),
                archived_session_stats_path: Some(archive.join("stats").display().to_string()),
                archived_compile_journal_path: Some(archive.join("compile").display().to_string()),
                private_daemon_name: None,
            }),
            miss_reasons: Vec::new(),
        }
    }

    #[test]
    fn age_size_active_and_database_contract() {
        let temp = tempfile::tempdir().unwrap();
        let paths = SoldrPaths::with_root(temp.path().join("owned"));
        let sibling = SoldrPaths::with_root(temp.path().join("sibling"));
        let db_path = paths.root.join("state.redb");
        let now = UNIX_EPOCH + Duration::from_secs(100 * 24 * 60 * 60);
        let now_ms = now.duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;
        for (id, age_days, active) in [
            (1_u64, 10_u64, false),
            (2, 3, false),
            (3, 2, false),
            (4, 90, true),
        ] {
            let dir = history_root(&paths).join(id.to_string());
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("payload"), b"x").unwrap();
            if active {
                mark_history_publishing(&dir).unwrap();
            } else {
                mark_history_complete(&dir).unwrap();
            }
            db::upsert_build(
                &db_path,
                &record(
                    id,
                    (!active).then_some(now_ms - age_days as i64 * 86_400_000),
                    &dir,
                ),
            )
            .unwrap();
        }
        let sibling_file = history_root(&sibling).join("9/sentinel");
        std::fs::create_dir_all(sibling_file.parent().unwrap()).unwrap();
        std::fs::write(&sibling_file, b"keep").unwrap();

        let options = HistoryGcOptions {
            now,
            max_age: DEFAULT_MAX_AGE,
            max_bytes: 1_200,
            migrate_pre_redaction: false,
        };
        let report = sweep_with_size(&paths, &db_path, &options, |_| 600);
        assert_eq!(report.age_removed, 1);
        assert_eq!(report.size_removed, 1);
        assert_eq!(report.protected_active, 1);
        assert!(history_root(&paths).join("3").is_dir());
        assert!(history_root(&paths).join("4").is_dir());
        assert!(sibling_file.is_file());
        let pruned = db::get_build(&db_path, 1).unwrap().unwrap();
        assert_eq!(
            pruned.log_paths.unwrap().archived_compile_journal_path,
            None
        );
    }

    #[test]
    fn pre_redaction_migration_removes_only_completed_legacy() {
        let temp = tempfile::tempdir().unwrap();
        let paths = SoldrPaths::with_root(temp.path().join("owned"));
        let db_path = paths.root.join("state.redb");
        for (id, active) in [(1_u64, false), (2, true)] {
            let dir = history_root(&paths).join(id.to_string());
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("raw-secret"), b"GITHUB_TOKEN=raw").unwrap();
            if active {
                mark_history_publishing(&dir).unwrap();
            }
            db::upsert_build(&db_path, &record(id, (!active).then_some(1), &dir)).unwrap();
        }
        let report = sweep(&paths, &db_path, &HistoryGcOptions::default());
        assert_eq!(report.migration_removed, 1);
        assert!(!history_root(&paths).join("1").exists());
        assert!(history_root(&paths).join("2/raw-secret").exists());
        assert!(!history_root(&paths)
            .join(SANITIZED_MIGRATION_MARKER)
            .exists());

        let active_dir = history_root(&paths).join("2");
        db::upsert_build(&db_path, &record(2, Some(2), &active_dir)).unwrap();
        mark_history_complete(&active_dir).unwrap();
        let report = sweep(&paths, &db_path, &HistoryGcOptions::default());
        assert_eq!(report.age_removed, 1);
        assert!(!active_dir.exists());
        assert!(history_root(&paths)
            .join(SANITIZED_MIGRATION_MARKER)
            .is_file());
    }

    #[test]
    fn legacy_session_file_migration_preserves_build_scoped_history() {
        // Regression for #1827: migrate only completed archives and keep
        // their build-scoped stats and sanitized compile journal.
        let temp = tempfile::tempdir().unwrap();
        let paths = SoldrPaths::with_root(temp.path().join("owned"));
        let db_path = paths.root.join("state.redb");
        let completed = history_root(&paths).join("31");
        let active = history_root(&paths).join("32");
        for archive in [&completed, &active] {
            std::fs::create_dir_all(archive).unwrap();
            std::fs::write(archive.join("last-session.log"), b"stale-secret").unwrap();
            std::fs::write(archive.join("last-session.jsonl"), b"stale-secret").unwrap();
            std::fs::write(archive.join("last-session-stats.json"), b"stats").unwrap();
            std::fs::write(archive.join("compile_journal.jsonl"), b"build-scoped").unwrap();
        }
        mark_history_complete(&completed).unwrap();
        mark_history_publishing(&active).unwrap();
        db::upsert_build(&db_path, &record(31, Some(1), &completed)).unwrap();
        db::upsert_build(&db_path, &record(32, Some(2), &active)).unwrap();

        let options = HistoryGcOptions {
            now: UNIX_EPOCH + Duration::from_millis(3),
            max_age: Duration::MAX,
            max_bytes: u64::MAX,
            migrate_pre_redaction: true,
        };
        let report = sweep(&paths, &db_path, &options);
        assert_eq!(report.legacy_files_removed, 2);
        assert_eq!(report.legacy_bytes_reclaimed, 24);
        assert!(!completed.join("last-session.log").exists());
        assert!(!completed.join("last-session.jsonl").exists());
        assert!(completed.join("last-session-stats.json").is_file());
        assert!(completed.join("compile_journal.jsonl").is_file());
        assert!(active.join("last-session.log").is_file());
        assert!(active.join("last-session.jsonl").is_file());
        assert!(!history_root(&paths)
            .join(LEGACY_SESSION_FILES_MIGRATION_MARKER)
            .exists());

        let completed_record = db::get_build(&db_path, 31).unwrap().unwrap();
        let completed_paths = completed_record.log_paths.unwrap();
        assert!(completed_paths.archived_session_log_path.is_none());
        assert!(completed_paths.archived_journal_path.is_none());
        assert!(completed_paths.archived_session_stats_path.is_some());
        assert!(completed_paths.archived_compile_journal_path.is_some());

        mark_history_complete(&active).unwrap();
        let report = sweep(&paths, &db_path, &options);
        assert_eq!(report.legacy_files_removed, 2);
        assert!(!active.join("last-session.log").exists());
        assert!(!active.join("last-session.jsonl").exists());
        assert!(history_root(&paths)
            .join(LEGACY_SESSION_FILES_MIGRATION_MARKER)
            .is_file());
    }

    #[test]
    fn legacy_session_file_migration_rejects_swapped_archive_link() {
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let paths = SoldrPaths::with_root(temp.path().join("owned"));
        let archive = history_root(&paths).join("41");
        let external = temp.path().join("external");
        std::fs::create_dir_all(&archive).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        std::fs::write(archive.join("last-session.log"), b"owned").unwrap();
        std::fs::write(external.join("last-session.log"), b"external").unwrap();

        // Simulate a replacement after enumeration but before the
        // migration unlinks a child artifact.
        std::fs::remove_dir_all(&archive).unwrap();
        crate::platform::fs::links::create(
            external.to_str().expect("UTF-8 test path"),
            &archive,
            true,
        )
        .unwrap();

        assert!(remove_legacy_session_file(&paths.root, &archive, "last-session.log").is_err());
        assert_eq!(
            std::fs::read(external.join("last-session.log")).unwrap(),
            b"external"
        );
    }

    #[test]
    fn abandoned_unfinished_database_row_does_not_block_retention() {
        let temp = tempfile::tempdir().unwrap();
        let paths = SoldrPaths::with_root(temp.path().join("owned"));
        let db_path = paths.root.join("state.redb");
        let archive = history_root(&paths).join("23");
        std::fs::create_dir_all(&archive).unwrap();
        std::fs::write(archive.join("pre-redaction"), b"secret").unwrap();
        db::upsert_build(&db_path, &record(23, None, &archive)).unwrap();

        let report = sweep(&paths, &db_path, &HistoryGcOptions::default());
        assert_eq!(report.migration_removed, 1);
        assert!(!archive.exists());
    }

    #[test]
    fn ended_database_row_does_not_expose_partial_publication() {
        let temp = tempfile::tempdir().unwrap();
        let paths = SoldrPaths::with_root(temp.path().join("owned"));
        let db_path = paths.root.join("state.redb");
        let archive = history_root(&paths).join("11");
        mark_history_publishing(&archive).unwrap();
        std::fs::write(archive.join("partial"), b"partial").unwrap();
        db::upsert_build(&db_path, &record(11, Some(1), &archive)).unwrap();

        let report = sweep(&paths, &db_path, &HistoryGcOptions::default());
        assert_eq!(report.protected_active, 1);
        assert_eq!(report.migration_removed, 0);
        assert!(archive.join("partial").is_file());
        assert!(!history_root(&paths)
            .join(SANITIZED_MIGRATION_MARKER)
            .exists());

        mark_history_complete(&archive).unwrap();
        assert!(!archive.join(PUBLISHING_MARKER).exists());
        assert!(archive.join(COMPLETE_MARKER).is_file());
    }

    #[test]
    fn publish_or_discard_drops_an_archive_with_no_payload() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("42");

        // The shape soldr#2186 produced: publication started, every copy
        // no-opped because its source was missing, nothing else written.
        mark_history_publishing(&archive).unwrap();
        assert!(!publish_or_discard(&archive).unwrap());
        assert!(
            !archive.exists(),
            "an archive with no payload must be removed, not published: {}",
            archive.display(),
        );

        // Withholding the marker instead would leak the directory, because
        // `sweep` reads a marker-less session as still-active.
        assert!(!archive.join(COMPLETE_MARKER).exists());
    }

    #[test]
    fn publish_or_discard_publishes_an_archive_with_payload() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("43");

        mark_history_publishing(&archive).unwrap();
        std::fs::write(archive.join("last-session-stats.json"), b"{}\n").unwrap();
        assert!(publish_or_discard(&archive).unwrap());
        assert!(archive.join(COMPLETE_MARKER).is_file());
        assert!(!archive.join(PUBLISHING_MARKER).exists());
        assert!(archive.join("last-session-stats.json").is_file());
    }

    #[test]
    fn publish_or_discard_tolerates_a_missing_archive() {
        let temp = tempfile::tempdir().unwrap();
        // Never created — a discarded archive must not become a hard error if
        // something else already removed it.
        assert!(!publish_or_discard(&temp.path().join("absent")).unwrap());
    }

    #[test]
    fn database_failures_block_success_markers() {
        let temp = tempfile::tempdir().unwrap();
        let paths = SoldrPaths::with_root(temp.path().join("owned"));
        let db_path = paths.root.join("state.redb");
        let options = HistoryGcOptions::default();
        let list_failed = sweep_with_ops(
            &paths,
            &db_path,
            &options,
            |_| 1,
            |_| Err("injected list failure".to_string()),
            |_, _| Ok(0),
        );
        assert_eq!(list_failed.failed, 1);
        assert!(!history_root(&paths)
            .join(SANITIZED_MIGRATION_MARKER)
            .exists());

        let archive = history_root(&paths).join("7");
        std::fs::create_dir_all(&archive).unwrap();
        std::fs::write(archive.join("legacy-secret"), b"secret").unwrap();
        let row = record(7, Some(1), &archive);
        let mark_failed = sweep_with_ops(
            &paths,
            &db_path,
            &options,
            |_| 1,
            |_| Ok(vec![row.clone()]),
            |_, _| Err("injected mark failure".to_string()),
        );
        assert_eq!(mark_failed.migration_removed, 0);
        assert_eq!(mark_failed.failed, 1);
        assert!(archive.is_dir());
        assert!(!history_root(&paths)
            .join(SANITIZED_MIGRATION_MARKER)
            .exists());
    }

    #[test]
    fn delete_failure_restores_archive_paths_for_retry() {
        let temp = tempfile::tempdir().unwrap();
        let paths = SoldrPaths::with_root(temp.path().join("owned"));
        let db_path = paths.root.join("state.redb");
        let archive = history_root(&paths).join("17");
        std::fs::create_dir_all(&archive).unwrap();
        std::fs::write(archive.join("payload"), b"keep").unwrap();
        mark_history_complete(&archive).unwrap();
        let row = record(17, Some(1), &archive);
        db::upsert_build(&db_path, &row).unwrap();

        let report = sweep_with_ops_and_remove(
            &paths,
            &db_path,
            &HistoryGcOptions {
                now: UNIX_EPOCH + Duration::from_secs(100 * 24 * 60 * 60),
                max_age: Duration::ZERO,
                max_bytes: u64::MAX,
                migrate_pre_redaction: false,
            },
            |_| 1,
            |path| db::list_builds(path, u32::MAX, None).map_err(|error| error.to_string()),
            |path, ids| db::mark_archives_unavailable(path, ids).map_err(|error| error.to_string()),
            |_| Err(std::io::Error::other("injected unlink failure")),
        );
        assert_eq!(report.failed, 1);
        assert_eq!(report.database_rows_updated, 0);
        assert!(archive.join("payload").is_file());
        let restored = db::get_build(&db_path, 17).unwrap().unwrap();
        assert!(restored
            .log_paths
            .unwrap()
            .archived_compile_journal_path
            .is_some());
    }

    #[test]
    fn linked_history_collection_is_retained() {
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let paths = SoldrPaths::with_root(temp.path().join("owned"));
        let external = temp.path().join("external");
        std::fs::create_dir_all(external.join("1")).unwrap();
        std::fs::create_dir_all(&paths.cache).unwrap();
        std::fs::create_dir_all(paths.cache.join("zccache")).unwrap();
        crate::platform::fs::links::create(
            external.to_str().expect("UTF-8 test path"),
            &history_root(&paths),
            true,
        )
        .unwrap();
        let report = sweep_with_ops(
            &paths,
            &paths.root.join("state.redb"),
            &HistoryGcOptions::default(),
            |_| 1,
            |_| Ok(Vec::new()),
            |_, _| Ok(0),
        );
        assert_eq!(report.failed, 1);
        assert!(external.join("1").is_dir());
    }
}
