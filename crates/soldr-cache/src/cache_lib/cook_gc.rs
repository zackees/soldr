//! `[cook]` auto-GC eviction pass (issue #589).
//!
//! Owns the policy + I/O for trimming `~/.soldr/cache/cook/` against
//! the `CookConfig` knobs in `~/.soldr/config.toml`. The cargo
//! front-door auto-GC orchestrator (`gc::maybe_kick_auto_gc`) drives
//! this on a throttled cadence so cook artifacts and quarantine files
//! never accumulate without bound.
//!
//! Algorithm (in order):
//!
//! 1. Collect every `(CookKey, CookEntry)` from `cook_index_v2`.
//! 2. Group by `origin_url_normalized` (None entries form one group).
//!    Inside each group, sort by `last_used_unix_ms` desc and mark
//!    the top `cfg.keep_per_origin` as PROTECTED — they are immune
//!    to both the time bound and the size cap.
//! 3. Time bound: evict every unprotected entry where
//!    `now - last_used_unix_ms > cfg.max_age_days`.
//! 4. Quarantine cleanup at the same time bound: delete any
//!    `*.tar.zst.quarantine` file whose mtime is older than
//!    `cfg.max_age_days`. Quarantine files are already a corruption
//!    signal so they get no protection.
//! 5. Size cap: if remaining `sum(size_bytes) > cfg.max_total_gb`,
//!    evict the unprotected entry with the smallest
//!    `last_used_unix_ms` until under cap.
//!
//! Every file / redb error is logged via `tracing::warn!` but does
//! not bubble up — the pass is best-effort.

use crate::cache_lib::cook_archive::{artifact_path_for_sha, cook_cache_dir};
use crate::cache_lib::{cook_index, state_db_path};
use crate::core::{CookConfig, SoldrPaths};
use std::collections::HashMap;

/// One GiB in bytes.
const GIB: u64 = 1024 * 1024 * 1024;

/// One day in milliseconds.
const MS_PER_DAY: u64 = 24 * 60 * 60 * 1000;

/// One day in seconds — used for quarantine mtime comparisons.
const SECS_PER_DAY: u64 = 24 * 60 * 60;

/// Diagnostic counters returned by [`cook_evict_pass`]. Useful for
/// the auto-GC log line and unit tests.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CookEvictReport {
    /// Total entries marked PROTECTED by `keep_per_origin`.
    pub protected: u32,
    /// Entries evicted because they exceeded the time bound.
    pub time_evicted: u32,
    /// Entries evicted because the size cap was breached.
    pub size_evicted: u32,
    /// Quarantine files deleted by the mtime sweep.
    pub quarantine_evicted: u32,
    /// Sum of `size_bytes` reclaimed across `time_evicted` +
    /// `size_evicted` (quarantine sizes are not tracked).
    pub bytes_freed: u64,
    /// Best-effort error counter — I/O failures, redb failures,
    /// missing-on-disk artifacts, etc.
    pub errors: u32,
}

/// Run the `[cook]` auto-GC eviction pass against `paths`.
///
/// Returns a [`CookEvictReport`] for diagnostics. All disk + redb
/// errors are absorbed (`tracing::warn!`) — the pass never panics or
/// bubbles a failure.
pub fn cook_evict_pass(paths: &SoldrPaths, cfg: &CookConfig) -> CookEvictReport {
    let mut report = CookEvictReport::default();
    let now_unix_ms = current_unix_ms();
    let db_path = state_db_path(paths);
    let cook_dir = cook_cache_dir(paths);

    let entries = match cook_index::iter_entries(&db_path) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "cook_evict_pass: failed to read cook_index");
            // Continue so the quarantine sweep still runs.
            Vec::new()
        }
    };

    // Group by origin (None lumps under a single empty key).
    let mut by_origin: HashMap<String, Vec<usize>> = HashMap::new();
    for (idx, (_, entry)) in entries.iter().enumerate() {
        let key = entry.origin_url_normalized.clone().unwrap_or_default();
        by_origin.entry(key).or_default().push(idx);
    }

    // Within each group, sort indices by last_used desc and mark the
    // top `keep_per_origin` as protected.
    let mut protected = vec![false; entries.len()];
    for indices in by_origin.values_mut() {
        indices.sort_by(|&a, &b| {
            entries[b]
                .1
                .last_used_unix_ms
                .cmp(&entries[a].1.last_used_unix_ms)
        });
        let keep = cfg.keep_per_origin as usize;
        for &idx in indices.iter().take(keep) {
            protected[idx] = true;
            report.protected = report.protected.saturating_add(1);
        }
    }

    // Time-bound eviction.
    let max_age_ms = cfg.max_age_days.saturating_mul(MS_PER_DAY);
    let mut evicted_indices: Vec<usize> = Vec::new();
    if max_age_ms > 0 {
        for (idx, (_, entry)) in entries.iter().enumerate() {
            if protected[idx] {
                continue;
            }
            let age_ms = now_unix_ms.saturating_sub(entry.last_used_unix_ms);
            if age_ms > max_age_ms && apply_eviction(&db_path, &cook_dir, entry.sha256, &mut report)
            {
                report.time_evicted = report.time_evicted.saturating_add(1);
                report.bytes_freed = report.bytes_freed.saturating_add(entry.size_bytes);
                evicted_indices.push(idx);
            }
        }
    }

    // Quarantine sweep — same time bound, no protection.
    let max_age_secs = cfg.max_age_days.saturating_mul(SECS_PER_DAY);
    if max_age_secs > 0 {
        report.quarantine_evicted = report.quarantine_evicted.saturating_add(sweep_quarantine(
            &cook_dir,
            max_age_secs,
            &mut report,
        ));
    }

    // Size cap: while sum(unprotected, surviving entries) > cap, evict LRU.
    let cap_bytes = cfg.max_total_gb.saturating_mul(GIB);
    if cap_bytes > 0 {
        // Mark already-evicted indices so they don't count toward total.
        let mut evicted_set: std::collections::HashSet<usize> =
            evicted_indices.iter().copied().collect();
        loop {
            let mut total: u64 = 0;
            for (i, (_, entry)) in entries.iter().enumerate() {
                if evicted_set.contains(&i) {
                    continue;
                }
                total = total.saturating_add(entry.size_bytes);
            }
            if total <= cap_bytes {
                break;
            }
            // Find unprotected LRU survivor.
            let victim = entries
                .iter()
                .enumerate()
                .filter(|(i, _)| !evicted_set.contains(i) && !protected[*i])
                .min_by_key(|(_, (_, entry))| entry.last_used_unix_ms);
            let Some((idx, (_, entry))) = victim else {
                // Nothing left to evict — only protected entries
                // remain. Log and stop; the pass is best-effort.
                break;
            };
            let sha = entry.sha256;
            let size = entry.size_bytes;
            if apply_eviction(&db_path, &cook_dir, sha, &mut report) {
                report.size_evicted = report.size_evicted.saturating_add(1);
                report.bytes_freed = report.bytes_freed.saturating_add(size);
                evicted_set.insert(idx);
            } else {
                // Avoid an infinite loop if eviction keeps failing.
                evicted_set.insert(idx);
            }
        }
    }

    report
}

/// Drop the row from `cook_index_v2` and unlink the on-disk
/// `<sha256>.tar.zst` file. Returns true on success. Best-effort —
/// any failure increments `report.errors`.
fn apply_eviction(
    db_path: &std::path::Path,
    cook_dir: &std::path::Path,
    sha256: [u8; 32],
    report: &mut CookEvictReport,
) -> bool {
    let removed_row = match cook_index::evict(db_path, &sha256) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "cook_evict_pass: redb evict failed");
            report.errors = report.errors.saturating_add(1);
            false
        }
    };
    let path = artifact_path_for_sha(cook_dir, &sha256);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "cook_evict_pass: unlink failed");
            report.errors = report.errors.saturating_add(1);
        }
    }
    removed_row
}

/// Delete any `*.tar.zst.quarantine` file in `cook_dir` whose mtime
/// is older than `max_age_secs`. Returns the count of files removed.
fn sweep_quarantine(
    cook_dir: &std::path::Path,
    max_age_secs: u64,
    report: &mut CookEvictReport,
) -> u32 {
    let entries = match std::fs::read_dir(cook_dir) {
        Ok(it) => it,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return 0,
        Err(e) => {
            tracing::warn!(error = %e, dir = %cook_dir.display(), "cook_evict_pass: read_dir cook failed");
            report.errors = report.errors.saturating_add(1);
            return 0;
        }
    };
    let now = std::time::SystemTime::now();
    let mut count: u32 = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with(".tar.zst.quarantine") {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        let age = match now.duration_since(modified) {
            Ok(d) => d,
            Err(_) => continue,
        };
        if age.as_secs() <= max_age_secs {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => count = count.saturating_add(1),
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "cook_evict_pass: quarantine unlink failed");
                report.errors = report.errors.saturating_add(1);
            }
        }
    }
    count
}

fn current_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    crate::timed_test!(report_starts_empty_on_unseeded_state, {
        let dir = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(dir.path().to_path_buf());
        paths.ensure_dirs().expect("ensure");
        let report = cook_evict_pass(&paths, &CookConfig::default());
        assert_eq!(report, CookEvictReport::default());
    });
}
