//! Install source cache + 2-day TTL sweep (soldr#2310).
//!
//! Source acquisitions (extracted codeload zips, `git clone --depth 1`)
//! land under `~/.soldr/cache/install/git/<host>/<owner>/<repo>@<sha>/`,
//! content-addressed by resolved commit sha so they are immutable and
//! safe to dedupe/delete. They are a **disposable** cache — a re-fetch is
//! seconds of bandwidth — so they carry a 2-day eager TTL and are the
//! first value-bearing category evicted under disk pressure.
//!
//! A `.partial` marker (mirroring the history publishing marker) means a
//! crashed/half acquisition is never mistaken for a complete cache entry.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::core::{SoldrError, SoldrPaths};

/// Env override for the source-cache TTL, in days.
pub(crate) const TTL_DAYS_ENV_VAR: &str = "SOLDR_INSTALL_SRC_TTL_DAYS";
/// Env kill switch for the source-cache TTL sweep.
pub(crate) const NO_GC_ENV_VAR: &str = "SOLDR_NO_INSTALL_SRC_GC";
/// Global floor: never delete an entry younger than one hour — a
/// concurrent install may still be using it. Mirrors
/// `AutoGcConfig::default_min_age_secs`.
pub(crate) const MIN_AGE_SECS: u64 = 3600;

const PARTIAL_MARKER: &str = ".partial";

/// Root of the install source cache: `<cache>/install/git`.
pub(crate) fn source_cache_root(paths: &SoldrPaths) -> PathBuf {
    paths.cache.join("install").join("git")
}

/// Content-addressed source cache dir for a resolved `(host, owner, repo, sha)`.
pub(crate) fn source_cache_dir(
    paths: &SoldrPaths,
    host: &str,
    owner: &str,
    repo: &str,
    sha: &str,
) -> PathBuf {
    source_cache_root(paths)
        .join(host)
        .join(owner)
        .join(format!("{repo}@{sha}"))
}

fn partial_marker(dir: &Path) -> PathBuf {
    dir.join(PARTIAL_MARKER)
}

/// A source cache dir is a hit only when it exists and carries no
/// `.partial` marker.
pub(crate) fn is_complete(dir: &Path) -> bool {
    dir.is_dir() && !partial_marker(dir).exists()
}

/// Mark an acquisition in-flight (drop the marker before writing bytes).
pub(crate) fn mark_partial(dir: &Path) -> Result<(), SoldrError> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(partial_marker(dir), b"in-flight\n")?;
    Ok(())
}

/// Publish a completed acquisition (remove the `.partial` marker).
pub(crate) fn clear_partial(dir: &Path) -> Result<(), SoldrError> {
    let marker = partial_marker(dir);
    if marker.exists() {
        std::fs::remove_file(marker)?;
    }
    Ok(())
}

/// Touch the entry's last-use time so the TTL sweep spares recently-used
/// clones. Best-effort — a failure to touch never fails the install.
pub(crate) fn touch_last_use(dir: &Path) {
    let stamp = dir.join(".last-use");
    let _ = std::fs::write(&stamp, b"");
}

/// TTL in seconds, honoring the env override then the config default.
pub(crate) fn ttl_secs(config_days: u64) -> u64 {
    let days = std::env::var(TTL_DAYS_ENV_VAR)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(config_days);
    days.saturating_mul(24 * 3600)
}

/// True when the sweep is disabled by env.
pub(crate) fn gc_disabled() -> bool {
    std::env::var_os(NO_GC_ENV_VAR).is_some_and(|v| {
        let v = v.to_string_lossy().trim().to_ascii_lowercase();
        matches!(v.as_str(), "1" | "true" | "yes" | "on")
    })
}

/// Pure eviction predicate: an entry is expired when its effective age
/// exceeds the TTL, but never when it is younger than the min-age floor.
pub(crate) fn entry_is_expired(effective_age_secs: u64, ttl_secs: u64, min_age_secs: u64) -> bool {
    effective_age_secs >= min_age_secs && effective_age_secs > ttl_secs
}

/// Effective age of a cache entry: the *newer* of the dir mtime and the
/// `.last-use` stamp, converted to seconds-ago. Conservative — a recent
/// touch always spares the entry, never over-deletes.
fn effective_age_secs(dir: &Path, now: SystemTime) -> Option<u64> {
    let dir_mtime = std::fs::metadata(dir).ok()?.modified().ok()?;
    let last_use = std::fs::metadata(dir.join(".last-use"))
        .ok()
        .and_then(|m| m.modified().ok());
    let newest = match last_use {
        Some(lu) if lu > dir_mtime => lu,
        _ => dir_mtime,
    };
    now.duration_since(newest)
        .ok()
        .map(|d| d.as_secs())
        .or(Some(0))
}

/// Sweep expired entries under the source cache root. Returns the number
/// of entries removed. In-flight (`.partial`) entries are always skipped.
pub(crate) fn sweep_expired(
    paths: &SoldrPaths,
    ttl_secs: u64,
    min_age_secs: u64,
    now: SystemTime,
) -> u64 {
    let root = source_cache_root(paths);
    if !root.is_dir() {
        return 0;
    }
    let mut removed = 0u64;
    // Layout: root/<host>/<owner>/<repo>@<sha>/
    for host in read_subdirs(&root) {
        for owner in read_subdirs(&host) {
            for entry in read_subdirs(&owner) {
                // Never evict an in-flight acquisition.
                if partial_marker(&entry).exists() {
                    continue;
                }
                let Some(age) = effective_age_secs(&entry, now) else {
                    continue;
                };
                if entry_is_expired(age, ttl_secs, min_age_secs)
                    && std::fs::remove_dir_all(&entry).is_ok()
                {
                    removed += 1;
                }
            }
        }
    }
    removed
}

fn read_subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    rd.filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .collect()
}

/// Convenience wrapper used by the offline GC sweeper: reads config +
/// env, then sweeps. Returns entries removed (0 when disabled).
pub(crate) fn sweep_with_config(paths: &SoldrPaths, config_days: u64) -> u64 {
    if gc_disabled() {
        return 0;
    }
    sweep_expired(
        paths,
        ttl_secs(config_days),
        MIN_AGE_SECS,
        SystemTime::now(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use filetime::FileTime;

    fn synthetic_paths(tmp: &Path) -> SoldrPaths {
        let root = tmp.join("soldr-home");
        std::fs::create_dir_all(&root).unwrap();
        SoldrPaths::with_root(root)
    }

    #[test]
    fn entry_is_expired_respects_ttl_and_min_age() {
        let ttl = 2 * 24 * 3600; // 2 days
        let min = MIN_AGE_SECS;
        // Fresh (< min age): never expired even past TTL is impossible here.
        assert!(!entry_is_expired(60, ttl, min));
        // Older than min but under TTL: not expired.
        assert!(!entry_is_expired(min + 10, ttl, min));
        // Older than TTL and past min: expired.
        assert!(entry_is_expired(ttl + 10, ttl, min));
        // Exactly at min age but under TTL: not expired.
        assert!(!entry_is_expired(min, ttl, min));
    }

    #[test]
    fn ttl_secs_env_overrides_config() {
        std::env::remove_var(TTL_DAYS_ENV_VAR);
        assert_eq!(ttl_secs(2), 2 * 24 * 3600);
        std::env::set_var(TTL_DAYS_ENV_VAR, "5");
        assert_eq!(ttl_secs(2), 5 * 24 * 3600);
        std::env::remove_var(TTL_DAYS_ENV_VAR);
    }

    #[test]
    fn partial_marker_hides_incomplete_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = synthetic_paths(tmp.path());
        let dir = source_cache_dir(&paths, "github.com", "o", "r", "abc123");
        mark_partial(&dir).unwrap();
        assert!(!is_complete(&dir), "a .partial entry is not a cache hit");
        clear_partial(&dir).unwrap();
        assert!(is_complete(&dir), "cleared entry is a cache hit");
    }

    #[test]
    fn sweep_removes_expired_but_keeps_fresh_and_partial() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = synthetic_paths(tmp.path());

        // An old, complete entry — should be swept.
        let old = source_cache_dir(&paths, "github.com", "o", "r", "old");
        std::fs::create_dir_all(&old).unwrap();
        let old_time = FileTime::from_unix_time(0, 0); // 1970 — very old
        filetime::set_file_mtime(&old, old_time).unwrap();

        // A fresh, complete entry — should survive.
        let fresh = source_cache_dir(&paths, "github.com", "o", "r", "fresh");
        std::fs::create_dir_all(&fresh).unwrap();

        // An old but in-flight entry — must be skipped by the sweeper.
        let inflight = source_cache_dir(&paths, "github.com", "o", "r", "inflight");
        mark_partial(&inflight).unwrap();
        filetime::set_file_mtime(&inflight, old_time).unwrap();

        let removed = sweep_expired(&paths, 2 * 24 * 3600, MIN_AGE_SECS, SystemTime::now());
        assert_eq!(removed, 1, "only the old complete entry is swept");
        assert!(!old.exists());
        assert!(fresh.exists());
        assert!(inflight.exists());
    }
}
