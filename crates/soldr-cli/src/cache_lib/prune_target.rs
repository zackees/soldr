//! Cargo `target/` artifact pruner: keep only the newest build per
//! `(parent_dir, prefix)` bucket inside `target/<profile>/{deps,
//! .fingerprint, incremental, build}/`.
//!
//! Implements the first slice of issue #316 — a manual subcommand,
//! `soldr cache prune-target <path>`. Auto pre/post-compile pruning via
//! `RUSTC_WRAPPER` hooks is deferred.
//!
//! ## Algorithm
//!
//! 1. Walk top-level entries in `target/<profile>/{deps, .fingerprint,
//!    incremental, build}/`.
//! 2. For each entry, look for a trailing `-[A-Za-z0-9]{13,}` suffix.
//!    Entries that don't match are left untouched.
//! 3. Strip the suffix to recover the prefix.
//! 4. Bucket by `(parent_dir, prefix)`. The same prefix appearing in
//!    `deps/`, `.fingerprint/`, `incremental/`, and `build/` is handled
//!    in independent buckets, because cargo tolerates orphans in any
//!    one of these.
//! 5. Within each bucket: sort by mtime descending, keep `[0]`, mark the
//!    rest for deletion.
//! 6. If `target/.cargo-lock` or any `target/<profile>/.cargo-lock` is
//!    present, refuse to run — that signals an active build.

use super::target_registry::RegistryError;
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

/// What this entry will be done to during this run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PruneAction {
    Keep,
    Delete,
}

/// One artifact entry discovered by the scan.
#[derive(Debug, Clone)]
pub struct PruneTargetEntry {
    /// Absolute path to the matched entry (file or directory).
    pub path: PathBuf,
    /// Recovered prefix (everything before the trailing `-<hash>`).
    pub prefix: String,
    /// The 13+char alphanumeric suffix.
    pub hash: String,
    /// Recursive on-disk size of the entry. Directories are summed.
    pub size_bytes: u64,
    /// Last-modified time in unix seconds. Falls back to 0 when
    /// metadata is unavailable.
    pub mtime_unix: i64,
    /// What this entry will be done to during this run.
    pub action: PruneAction,
}

/// Options that drive [`prune_target`].
#[derive(Debug, Clone)]
pub struct PruneTargetOptions {
    /// Path to the cargo `target/` directory to scan.
    pub target_dir: PathBuf,
    /// When `true`, no entries are deleted. The returned report still
    /// describes the plan.
    pub dry_run: bool,
}

impl PruneTargetOptions {
    pub fn new(target_dir: PathBuf) -> Self {
        Self {
            target_dir,
            dry_run: true,
        }
    }
}

/// Aggregated scan + delete result.
#[derive(Debug, Clone, Default)]
pub struct PruneTargetReport {
    /// Number of hash-suffixed entries the scan considered.
    pub scanned: usize,
    /// Number of entries kept (newest of each bucket).
    pub kept: usize,
    /// Number of entries deleted (or that would be deleted in a
    /// dry-run).
    pub deleted: usize,
    /// Bytes reclaimed by the actual or planned deletions.
    pub reclaimed_bytes: u64,
    /// Every classified entry, in the order they were scanned.
    pub entries: Vec<PruneTargetEntry>,
}

/// Scan a cargo `target/` directory and prune stale per-prefix
/// artifacts, keeping only the newest build of each `(parent_dir,
/// prefix)` bucket.
///
/// Idempotent: a second run on the same directory is a no-op (every
/// remaining bucket has a single entry, so nothing is deletable).
pub fn prune_target(opts: &PruneTargetOptions) -> Result<PruneTargetReport, RegistryError> {
    let target_dir = &opts.target_dir;
    let metadata = fs::metadata(target_dir).map_err(RegistryError::Io)?;
    if !metadata.is_dir() {
        return Err(RegistryError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "prune-target requires a directory: {}",
                target_dir.display()
            ),
        )));
    }

    if let Some(lock) = find_active_cargo_lock(target_dir) {
        return Err(RegistryError::Io(std::io::Error::other(format!(
            "cargo lock present at {} (active build?); refusing to prune",
            lock.display()
        ))));
    }

    let scan_dirs = collect_scan_dirs(target_dir);
    let mut entries: Vec<PruneTargetEntry> = Vec::new();
    let mut buckets: HashMap<(PathBuf, String), Vec<usize>> = HashMap::new();

    for parent in &scan_dirs {
        let read_dir = match fs::read_dir(parent) {
            Ok(it) => it,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(RegistryError::Io(err)),
        };
        for dirent in read_dir {
            let dirent = match dirent {
                Ok(d) => d,
                Err(err) => return Err(RegistryError::Io(err)),
            };
            let path = dirent.path();
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            if name == ".cargo-lock" {
                // Defensive: collect_scan_dirs already filters by parent,
                // but skip explicitly so we never even consider the lock.
                continue;
            }
            let Some((prefix, hash)) = split_hash_suffix(&name) else {
                continue;
            };
            let file_metadata = match dirent.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let size_bytes = entry_size_bytes(&path, &file_metadata);
            let mtime_unix = entry_mtime_unix(&file_metadata);
            let entry = PruneTargetEntry {
                path,
                prefix: prefix.to_string(),
                hash: hash.to_string(),
                size_bytes,
                mtime_unix,
                action: PruneAction::Keep,
            };
            let idx = entries.len();
            buckets
                .entry((parent.clone(), prefix.to_string()))
                .or_default()
                .push(idx);
            entries.push(entry);
        }
    }

    // Classify within each bucket: newest mtime wins; if mtime ties,
    // fall back to the lexicographically largest hash so the ordering
    // is deterministic.
    for indices in buckets.values_mut() {
        indices.sort_by(|a, b| {
            let ea = &entries[*a];
            let eb = &entries[*b];
            eb.mtime_unix
                .cmp(&ea.mtime_unix)
                .then_with(|| eb.hash.cmp(&ea.hash))
        });
        let mut iter = indices.iter().copied();
        if let Some(keep) = iter.next() {
            entries[keep].action = PruneAction::Keep;
        }
        for idx in iter {
            entries[idx].action = PruneAction::Delete;
        }
    }

    let mut report = PruneTargetReport {
        scanned: entries.len(),
        ..PruneTargetReport::default()
    };

    if !opts.dry_run {
        for entry in &entries {
            if entry.action != PruneAction::Delete {
                continue;
            }
            delete_entry(&entry.path).map_err(RegistryError::Io)?;
        }
    }

    for entry in &entries {
        match entry.action {
            PruneAction::Keep => report.kept += 1,
            PruneAction::Delete => {
                report.deleted += 1;
                report.reclaimed_bytes = report.reclaimed_bytes.saturating_add(entry.size_bytes);
            }
        }
    }
    report.entries = entries;
    Ok(report)
}

/// Standard cargo-managed subdirectories of `target/<profile>/` where
/// hash-suffixed names live. We also scan the `target/<profile>/build/`
/// directory because per-build-script output directories carry the
/// same suffix layout.
const SUBDIRS_WITH_HASH_SUFFIXES: &[&str] = &["deps", ".fingerprint", "incremental", "build"];

/// Files cargo writes that the wrapper must NEVER strip from an
/// archived `target/` snapshot. Removing any of these on the load side
/// invalidates the entire snapshot in cargo's eyes — dropping
/// `.rustc_info.json` forces a full rebuild because cargo re-probes the
/// toolchain on the next invocation.
///
/// Patterns are documentary only; the trim path enumerates them
/// explicitly rather than glob-matching. Keep this list and the
/// trim-target implementation in sync.
pub const MUST_KEEP_BIT_EXACT: &[&str] = &[
    "CACHEDIR.TAG",
    ".rustc_info.json",
    ".fingerprint/*/dep-*",
    ".fingerprint/*/output-*",
    ".fingerprint/*/invoked.timestamp",
];

/// Files that must NEVER appear inside an archived `target/` snapshot.
/// `.cargo-lock` is a transient flock sentinel — replaying it on the
/// load side would falsely signal an in-flight build to cargo. The
/// pre-archive trim path explicitly skips these names; the existing
/// [`find_active_cargo_lock`] also refuses to prune while one is
/// present locally.
pub const NEVER_ARCHIVE: &[&str] = &[".cargo-lock"];

/// Profile-relative subdirectory whose contents are rustc's incremental
/// compilation database. Excluded from CI-profile archives because
/// rustc resets the incremental session on every fresh runner and
/// the per-process cache contributes ~0% hit rate against the next
/// CI job's invocation.
pub const INCREMENTAL_SUBDIR: &str = "incremental";

fn collect_scan_dirs(target_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let profiles = match fs::read_dir(target_dir) {
        Ok(it) => it,
        Err(_) => return out,
    };
    for entry in profiles.flatten() {
        let profile_path = entry.path();
        let metadata = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if !metadata.is_dir() {
            continue;
        }
        for sub in SUBDIRS_WITH_HASH_SUFFIXES {
            let candidate = profile_path.join(sub);
            if candidate.is_dir() {
                out.push(candidate);
            }
        }
    }
    out
}

/// Walk the `target/` directory looking for an active cargo lock file.
/// Returns the first match, if any.
fn find_active_cargo_lock(target_dir: &Path) -> Option<PathBuf> {
    let top_lock = target_dir.join(".cargo-lock");
    if top_lock.exists() {
        return Some(top_lock);
    }
    let profiles = fs::read_dir(target_dir).ok()?;
    for entry in profiles.flatten() {
        if !entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
            continue;
        }
        let candidate = entry.path().join(".cargo-lock");
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Recognize cargo's `<prefix>-<hash>` naming. The hash is 13+ chars
/// long, ASCII-alphanumeric, and is the suffix after the **last** `-`
/// in the name. Returns `(prefix, hash)` when matched, or `None`.
///
/// File extensions (e.g. `.rlib`, `.d`, `-<hash>.json`) are stripped
/// before matching so `libfoo-abc1234567890.rlib` collapses into
/// prefix `libfoo` with hash `abc1234567890`. The leading `lib` prefix
/// itself is *not* stripped — `libfoo` and `foo` live in different
/// buckets because cargo names them that way on disk.
pub fn split_hash_suffix(name: &str) -> Option<(&str, &str)> {
    let base = match name.rfind('.') {
        Some(dot) if dot > 0 => &name[..dot],
        _ => name,
    };
    let dash = base.rfind('-')?;
    if dash == 0 {
        return None;
    }
    let prefix = &base[..dash];
    let hash = &base[dash + 1..];
    if hash.len() < 13 {
        return None;
    }
    if !hash.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some((prefix, hash))
}

fn entry_size_bytes(path: &Path, metadata: &fs::Metadata) -> u64 {
    if metadata.is_file() {
        return metadata.len();
    }
    if metadata.is_dir() {
        return directory_size(path);
    }
    0
}

fn directory_size(path: &Path) -> u64 {
    let mut total: u64 = 0;
    let mut stack: Vec<PathBuf> = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let read_dir = match fs::read_dir(&dir) {
            Ok(it) => it,
            Err(_) => continue,
        };
        for entry in read_dir.flatten() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_dir() {
                stack.push(entry.path());
            } else if metadata.is_file() {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    total
}

fn entry_mtime_unix(metadata: &fs::Metadata) -> i64 {
    let modified = match metadata.modified() {
        Ok(t) => t,
        Err(_) => return 0,
    };
    match modified.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(_) => 0,
    }
}

fn delete_entry(path: &Path) -> Result<(), std::io::Error> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    if metadata.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// Convert a unix timestamp to a [`std::time::SystemTime`].
#[cfg(test)]
fn system_time_from_unix(seconds: u64) -> std::time::SystemTime {
    UNIX_EPOCH + std::time::Duration::from_secs(seconds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use tempfile::tempdir;

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        File::create(path).unwrap();
    }

    fn touch_dir(path: &Path) {
        fs::create_dir_all(path).unwrap();
    }

    /// Set the mtime on a file. This is intentionally file-only because
    /// `set_modified` on a directory handle requires privileged access
    /// on Windows.
    fn set_mtime(path: &Path, unix_seconds: u64) {
        let when = system_time_from_unix(unix_seconds);
        let f = File::options()
            .write(true)
            .open(path)
            .expect("set_mtime: open writable handle");
        f.set_modified(when).expect("set_mtime: set_modified");
    }

    #[test]
    fn split_hash_suffix_recognizes_directory_names() {
        let parsed = split_hash_suffix("foo-abc1234567890");
        assert_eq!(parsed, Some(("foo", "abc1234567890")));
    }

    #[test]
    fn split_hash_suffix_recognizes_compound_names() {
        let parsed = split_hash_suffix("zccache_daemon-3ohgys91001go");
        assert_eq!(parsed, Some(("zccache_daemon", "3ohgys91001go")));
    }

    #[test]
    fn split_hash_suffix_strips_known_extensions() {
        let parsed = split_hash_suffix("libfoo-abcdef1234567.rlib");
        assert_eq!(parsed, Some(("libfoo", "abcdef1234567")));
    }

    #[test]
    fn split_hash_suffix_rejects_short_hashes() {
        assert!(split_hash_suffix("foo-abc123").is_none());
    }

    #[test]
    fn split_hash_suffix_rejects_non_alphanumeric() {
        assert!(split_hash_suffix("foo-bar.baz_quux12345").is_none());
    }

    #[test]
    fn split_hash_suffix_rejects_no_dash() {
        assert!(split_hash_suffix("no_hash_here_at_all").is_none());
    }

    #[test]
    fn single_entry_prefix_is_kept() {
        let temp = tempdir().unwrap();
        let target = temp.path().to_path_buf();
        let single = target
            .join("debug")
            .join(".fingerprint")
            .join("foo-abc1234567890");
        touch_dir(&single);

        let report = prune_target(&PruneTargetOptions {
            target_dir: target,
            dry_run: false,
        })
        .unwrap();

        assert_eq!(report.scanned, 1);
        assert_eq!(report.kept, 1);
        assert_eq!(report.deleted, 0);
        assert!(single.exists());
    }

    #[test]
    fn multi_entry_prefix_keeps_newest() {
        let temp = tempdir().unwrap();
        let target = temp.path().to_path_buf();
        let parent = target.join("debug").join("deps");
        // Use file entries (.rlib) so mtime can be pinned deterministically
        // on Windows, where set_modified on a directory requires
        // privileged access.
        let oldest = parent.join("libfoo-aaaaaaaaaaaaa.rlib");
        let middle = parent.join("libfoo-bbbbbbbbbbbbb.rlib");
        let newest = parent.join("libfoo-ccccccccccccc.rlib");
        for f in [&oldest, &middle, &newest] {
            touch(f);
        }
        set_mtime(&oldest, 1_700_000_000);
        set_mtime(&middle, 1_700_000_100);
        set_mtime(&newest, 1_700_000_200);

        let report = prune_target(&PruneTargetOptions {
            target_dir: target,
            dry_run: false,
        })
        .unwrap();

        assert_eq!(report.scanned, 3);
        assert_eq!(report.kept, 1);
        assert_eq!(report.deleted, 2);
        assert!(newest.exists(), "newest file must survive");
        assert!(!oldest.exists(), "oldest must be deleted");
        assert!(!middle.exists(), "middle must be deleted");
    }

    #[test]
    fn non_matching_names_untouched() {
        let temp = tempdir().unwrap();
        let target = temp.path().to_path_buf();
        // No hash suffix: should never be scanned.
        let script = target.join("debug").join("build-script.txt");
        touch(&script);
        // Looks like a name with hash but lives at top level (not under
        // a known subdir): should be ignored.
        let stray = target.join("debug").join("loose-aaaaaaaaaaaaa");
        touch_dir(&stray);

        let report = prune_target(&PruneTargetOptions {
            target_dir: target,
            dry_run: false,
        })
        .unwrap();
        assert_eq!(report.scanned, 0, "no entries should match the scan dirs");
        assert!(script.exists());
        assert!(stray.exists());
    }

    #[test]
    fn buckets_are_per_subdirectory() {
        let temp = tempdir().unwrap();
        let target = temp.path().to_path_buf();
        let fingerprint = target
            .join("debug")
            .join(".fingerprint")
            .join("foo-aaaaaaaaaaaaa");
        let deps = target
            .join("debug")
            .join("deps")
            .join("foo-bbbbbbbbbbbbb.rlib");
        let incremental = target
            .join("debug")
            .join("incremental")
            .join("foo-ccccccccccccc");
        touch_dir(&fingerprint);
        touch(&deps);
        touch_dir(&incremental);

        let report = prune_target(&PruneTargetOptions {
            target_dir: target,
            dry_run: false,
        })
        .unwrap();
        assert_eq!(report.scanned, 3);
        assert_eq!(report.kept, 3);
        assert_eq!(report.deleted, 0);
        assert!(fingerprint.exists());
        assert!(deps.exists());
        assert!(incremental.exists());
    }

    #[test]
    fn dry_run_does_not_delete() {
        let temp = tempdir().unwrap();
        let target = temp.path().to_path_buf();
        let parent = target.join("debug").join("deps");
        let older = parent.join("libfoo-aaaaaaaaaaaaa.rlib");
        let newer = parent.join("libfoo-bbbbbbbbbbbbb.rlib");
        touch(&older);
        touch(&newer);
        set_mtime(&older, 1_700_000_000);
        set_mtime(&newer, 1_700_000_500);

        let report = prune_target(&PruneTargetOptions {
            target_dir: target,
            dry_run: true,
        })
        .unwrap();
        assert_eq!(report.scanned, 2);
        assert_eq!(report.kept, 1);
        assert_eq!(report.deleted, 1);
        // Files must remain on disk because we asked for dry-run.
        assert!(older.exists(), "dry-run must not delete older entry");
        assert!(newer.exists(), "dry-run must not delete newer entry");
    }

    #[test]
    fn refuses_when_cargo_lock_present() {
        let temp = tempdir().unwrap();
        let target = temp.path().to_path_buf();
        let parent = target.join("debug").join(".fingerprint");
        let entry = parent.join("foo-aaaaaaaaaaaaa");
        touch_dir(&entry);
        touch(&target.join(".cargo-lock"));

        let err = prune_target(&PruneTargetOptions {
            target_dir: target.clone(),
            dry_run: false,
        })
        .expect_err("must refuse when top-level lock exists");
        assert!(format!("{err}").contains(".cargo-lock"));
        assert!(entry.exists(), "no entries may be touched when refusing");
        assert!(target.join(".cargo-lock").exists(), "lock must survive");
    }

    #[test]
    fn refuses_when_profile_cargo_lock_present() {
        let temp = tempdir().unwrap();
        let target = temp.path().to_path_buf();
        let parent = target.join("debug").join(".fingerprint");
        let entry = parent.join("foo-aaaaaaaaaaaaa");
        touch_dir(&entry);
        touch(&target.join("debug").join(".cargo-lock"));

        let err = prune_target(&PruneTargetOptions {
            target_dir: target,
            dry_run: false,
        })
        .expect_err("must refuse when profile lock exists");
        assert!(format!("{err}").contains(".cargo-lock"));
        assert!(entry.exists(), "entry must survive a refusal");
    }

    #[test]
    fn second_run_is_a_no_op() {
        let temp = tempdir().unwrap();
        let target = temp.path().to_path_buf();
        let parent = target.join("debug").join("deps");
        let older = parent.join("libfoo-aaaaaaaaaaaaa.rlib");
        let newer = parent.join("libfoo-bbbbbbbbbbbbb.rlib");
        touch(&older);
        touch(&newer);
        set_mtime(&older, 1_700_000_000);
        set_mtime(&newer, 1_700_000_500);

        let _ = prune_target(&PruneTargetOptions {
            target_dir: target.clone(),
            dry_run: false,
        })
        .unwrap();
        let second = prune_target(&PruneTargetOptions {
            target_dir: target,
            dry_run: false,
        })
        .unwrap();
        assert_eq!(second.scanned, 1);
        assert_eq!(second.kept, 1);
        assert_eq!(second.deleted, 0);
    }
}
