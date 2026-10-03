//! The phases of a build-history sweep, in their original execution order.

use super::{
    contains_legacy_session_files, remove_legacy_session_file, system_time_from_millis, Entry,
    HistoryGcOptions, HistoryGcReport, ABANDONED_PUBLISHING_MAX_AGE, COMPLETE_MARKER,
    LEGACY_SESSION_FILES_MIGRATION_MARKER, LEGACY_SESSION_FILE_NAMES, PUBLISHING_MARKER,
    SANITIZED_MIGRATION_MARKER,
};
use crate::{
    core::SoldrPaths,
    daemon::{db, protocol::BuildRecord},
};
use std::{collections::HashMap, path::Path};

pub(super) fn scan<F>(
    paths: &SoldrPaths,
    root: &Path,
    records: &HashMap<u64, BuildRecord>,
    options: &HistoryGcOptions,
    size_of: &mut F,
    report: &mut HistoryGcReport,
) -> Option<Vec<Entry>>
where
    F: FnMut(&Path) -> u64,
{
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return None;
        }
        Err(_) => {
            report.failed = 1;
            return None;
        }
    };
    if crate::cache_lib::path_safety::validate_owned_directory(&paths.root, root).is_err() {
        report.failed = 1;
        return None;
    }
    let mut candidates = Vec::new();
    for entry in entries {
        if let Some(entry) = candidate(entry, records, options, size_of, report) {
            candidates.push(entry);
        }
    }
    Some(candidates)
}

fn candidate<F>(
    entry: std::io::Result<std::fs::DirEntry>,
    records: &HashMap<u64, BuildRecord>,
    options: &HistoryGcOptions,
    size_of: &mut F,
    report: &mut HistoryGcReport,
) -> Option<Entry>
where
    F: FnMut(&Path) -> u64,
{
    let Ok(entry) = entry else {
        report.failed += 1;
        return None;
    };
    let Ok(kind) = std::fs::symlink_metadata(entry.path()) else {
        report.failed += 1;
        return None;
    };
    if crate::cache_lib::path_safety::is_link_or_reparse(&kind) {
        report.failed += 1;
        return None;
    }
    if !kind.is_dir() {
        return None;
    }
    let session_id = entry
        .file_name()
        .to_str()
        .and_then(|name| name.parse().ok())?;
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
                    return None;
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(_) => {
                report.failed += 1;
                return None;
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
        return None;
    };
    let bytes = size_of(&path);
    report.bytes_before = report.bytes_before.saturating_add(bytes);
    Some(Entry {
        session_id,
        path,
        bytes,
        completed_at,
        active,
    })
}

pub(super) fn migrate_legacy(
    paths: &SoldrPaths,
    db_path: &Path,
    candidates: &mut [Entry],
    records: &mut HashMap<u64, BuildRecord>,
    report: &mut HistoryGcReport,
) -> bool {
    let mut legacy_migration_pending = false;
    let mut rows_to_clear = Vec::new();
    for entry in candidates {
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
                report.database_rows_updated = report.database_rows_updated.saturating_add(updated);
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
    legacy_migration_pending
}

pub(super) fn select(
    candidates: &mut [Entry],
    options: &HistoryGcOptions,
    report: &HistoryGcReport,
    migration_due: bool,
) -> HashMap<u64, &'static str> {
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
    for entry in candidates.iter() {
        if entry.active || selected.contains_key(&entry.session_id) {
            continue;
        }
        if bytes_after_plan <= options.max_bytes {
            break;
        }
        selected.insert(entry.session_id, "size");
        bytes_after_plan = bytes_after_plan.saturating_sub(entry.bytes);
    }

    selected
}

pub(super) fn remove<M, D>(
    candidates: &[Entry],
    selected: &HashMap<u64, &'static str>,
    records: &HashMap<u64, BuildRecord>,
    db_path: &Path,
    mark_unavailable: &mut M,
    remove_dir: &mut D,
    report: &mut HistoryGcReport,
) -> Vec<u64>
where
    M: FnMut(&Path, &[u64]) -> Result<u64, String>,
    D: FnMut(&Path) -> std::io::Result<()>,
{
    let mut removed_ids = Vec::new();
    for entry in candidates {
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
    removed_ids
}

pub(super) fn finish(
    root: &Path,
    candidates: &[Entry],
    removed_ids: &[u64],
    migration_due: bool,
    legacy_migration_due: bool,
    legacy_migration_pending: bool,
    report: &mut HistoryGcReport,
) {
    report.bytes_after = report.bytes_before.saturating_sub(report.bytes_reclaimed);
    let migration_pending = candidates.iter().any(|entry| {
        !entry.path.join(COMPLETE_MARKER).is_file() && !removed_ids.contains(&entry.session_id)
    });
    if migration_due
        && report.failed == 0
        && !migration_pending
        && std::fs::create_dir_all(root)
            .and_then(|()| std::fs::write(root.join(SANITIZED_MIGRATION_MARKER), b"complete\n"))
            .is_err()
    {
        report.failed += 1;
    }
    if legacy_migration_due
        && report.failed == 0
        && !legacy_migration_pending
        && std::fs::create_dir_all(root)
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
}
