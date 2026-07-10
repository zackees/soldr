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

/// Provenance of the recency timestamp used to rank a hash family.
/// Recorded on each [`PruneTargetReport`] so consumers can tell when
/// the keep decision used cargo's authoritative
/// `.fingerprint/<prefix>-<hash>/invoked.timestamp` vs. a fallback to
/// the entry's own mtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecencySource {
    /// Recency came from `.fingerprint/<prefix>-<hash>/invoked.timestamp`,
    /// cargo's authoritative per-artifact "last invoked" record.
    FingerprintInvokedTimestamp,
    /// Recency came from the entry's own filesystem mtime — used when
    /// the matching `invoked.timestamp` file is missing or unreadable.
    EntryMtime,
}

impl RecencySource {
    pub fn as_str(self) -> &'static str {
        match self {
            RecencySource::FingerprintInvokedTimestamp => "fingerprint_invoked_timestamp",
            RecencySource::EntryMtime => "entry_mtime",
        }
    }
}

/// Options that drive [`prune_target`].
#[derive(Debug, Clone)]
pub struct PruneTargetOptions {
    /// Path to the cargo `target/` directory to scan.
    pub target_dir: PathBuf,
    /// When `true`, no entries are deleted. The returned report still
    /// describes the plan.
    pub dry_run: bool,
    /// When `true`, switch from the per-`(parent_dir, prefix)` bucket
    /// strategy (issue #336 — drop orphan siblings inside each subdir)
    /// to the aggressive per-`prefix` strategy (issue #316 — keep only
    /// the **newest hash family** per logical artifact name, deleting
    /// every other hash's files across `deps/`, `.fingerprint/`,
    /// `incremental/`, and `build/`).
    ///
    /// The aggressive mode is what shrinks a heavily-rebuilt target/
    /// from 16+ GB to ~2 GB on real workloads (issue #316). It is
    /// destructive but bounded: the winning hash is the one with the
    /// freshest mtime, and the active-`.cargo-lock` guard still
    /// applies.
    pub keep_latest: bool,
}

impl PruneTargetOptions {
    pub fn new(target_dir: PathBuf) -> Self {
        Self {
            target_dir,
            dry_run: true,
            keep_latest: false,
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
    /// How many of the keep decisions used cargo's authoritative
    /// `.fingerprint/<prefix>-<hash>/invoked.timestamp` mtime (the
    /// same signal cargo's own `-Zgc` consults). The remainder fell
    /// back to the entry's own filesystem mtime.
    pub keep_decisions_from_fingerprint: usize,
    pub keep_decisions_from_mtime: usize,
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
            entries.push(entry);
        }
    }

    let mut fingerprint_decisions = 0usize;
    let mut mtime_decisions = 0usize;

    if opts.keep_latest {
        classify_per_prefix(
            &mut entries,
            target_dir,
            &mut fingerprint_decisions,
            &mut mtime_decisions,
        );
    } else {
        classify_per_parent_prefix(
            &mut entries,
            target_dir,
            &mut fingerprint_decisions,
            &mut mtime_decisions,
        );
    }

    let mut report = PruneTargetReport {
        scanned: entries.len(),
        keep_decisions_from_fingerprint: fingerprint_decisions,
        keep_decisions_from_mtime: mtime_decisions,
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

/// Classic per-`(parent_dir, prefix)` bucketing (issue #336). Each
/// subdirectory's set of hash-siblings is pruned independently, so
/// the same prefix in `deps/`, `.fingerprint/`, etc. keeps its own
/// newest entry. Conservative: cargo tolerates orphans across the
/// four subdirs as long as the live entry inside each is current.
fn classify_per_parent_prefix(
    entries: &mut [PruneTargetEntry],
    target_dir: &Path,
    fingerprint_decisions: &mut usize,
    mtime_decisions: &mut usize,
) {
    let mut buckets: HashMap<(PathBuf, String), Vec<usize>> = HashMap::new();
    for (idx, entry) in entries.iter().enumerate() {
        let parent = entry
            .path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_default();
        buckets
            .entry((parent, entry.prefix.clone()))
            .or_default()
            .push(idx);
    }
    for indices in buckets.values_mut() {
        rank_indices(
            entries,
            indices,
            target_dir,
            fingerprint_decisions,
            mtime_decisions,
        );
        let mut iter = indices.iter().copied();
        if let Some(keep) = iter.next() {
            entries[keep].action = PruneAction::Keep;
        }
        for idx in iter {
            entries[idx].action = PruneAction::Delete;
        }
    }
}

/// Aggressive per-`prefix` bucketing (issue #316 — `--keep-latest`).
/// All entries sharing a prefix across `deps/`, `.fingerprint/`,
/// `incremental/`, and `build/` form a single bucket. The newest
/// **hash family** wins — every entry whose hash matches the winning
/// hash is kept; entries with any other hash are deleted.
///
/// Cargo expects the four subdirs to stay consistent (a `deps/` entry
/// for hash X must have a matching `.fingerprint/<prefix>-X/`), so
/// keeping the whole family avoids partially-orphaned state.
fn classify_per_prefix(
    entries: &mut [PruneTargetEntry],
    target_dir: &Path,
    fingerprint_decisions: &mut usize,
    mtime_decisions: &mut usize,
) {
    let mut buckets: HashMap<String, Vec<usize>> = HashMap::new();
    for (idx, entry) in entries.iter().enumerate() {
        buckets.entry(entry.prefix.clone()).or_default().push(idx);
    }
    for indices in buckets.values_mut() {
        rank_indices(
            entries,
            indices,
            target_dir,
            fingerprint_decisions,
            mtime_decisions,
        );
        let winning_hash = match indices.first() {
            Some(&idx) => entries[idx].hash.clone(),
            None => continue,
        };
        for &idx in indices.iter() {
            if entries[idx].hash == winning_hash {
                entries[idx].action = PruneAction::Keep;
            } else {
                entries[idx].action = PruneAction::Delete;
            }
        }
    }
}

/// Sort `indices` (offsets into `entries`) descending by the
/// authoritative recency timestamp, falling back to the entry's own
/// mtime. Within an equal timestamp the lexicographically larger hash
/// wins, so the ordering is deterministic across filesystems with
/// coarse mtime resolution.
///
/// Increments the appropriate counter for each NEWEST-entry decision
/// based on which timestamp source supplied its rank.
fn rank_indices(
    entries: &[PruneTargetEntry],
    indices: &mut Vec<usize>,
    target_dir: &Path,
    fingerprint_decisions: &mut usize,
    mtime_decisions: &mut usize,
) {
    let ranked: Vec<(usize, i64, RecencySource)> = indices
        .iter()
        .map(|&idx| {
            let entry = &entries[idx];
            let (ts, source) = entry_recency(entry, target_dir);
            (idx, ts, source)
        })
        .collect();
    let mut sorted = ranked;
    sorted.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| entries[b.0].hash.cmp(&entries[a.0].hash))
    });
    if let Some(&(_, _, source)) = sorted.first() {
        match source {
            RecencySource::FingerprintInvokedTimestamp => *fingerprint_decisions += 1,
            RecencySource::EntryMtime => *mtime_decisions += 1,
        }
    }
    *indices = sorted.into_iter().map(|(idx, _, _)| idx).collect();
}

/// Resolve the recency timestamp for one entry. Prefers cargo's
/// authoritative `.fingerprint/<crate>-<hash>/invoked.timestamp`
/// mtime, since cargo touches it on every invocation that consults
/// the artifact; falls back to the entry's own mtime when the
/// fingerprint file is missing or unreadable.
///
/// Cargo names `.fingerprint/` dirs after the unadorned crate name
/// (e.g. `foo-<hash>`), but the corresponding `deps/` entries carry
/// a `lib` prefix for `rlib`/`dylib` targets (e.g.
/// `libfoo-<hash>.rlib`). We look up `<prefix>-<hash>` first; if
/// `<prefix>` starts with `lib` and that misses, we retry against
/// `<prefix without lib>-<hash>`.
pub(crate) fn entry_recency(entry: &PruneTargetEntry, target_dir: &Path) -> (i64, RecencySource) {
    let lib_stripped = entry.prefix.strip_prefix("lib");
    let candidates: &[&str] = match lib_stripped {
        Some(stripped) => &[entry.prefix.as_str(), stripped],
        None => &[entry.prefix.as_str()],
    };
    let profiles = match fs::read_dir(target_dir) {
        Ok(it) => it,
        Err(_) => return (entry.mtime_unix, RecencySource::EntryMtime),
    };
    let profile_paths: Vec<PathBuf> = profiles.flatten().map(|e| e.path()).collect();
    for candidate_prefix in candidates {
        let stem = format!("{candidate_prefix}-{}", entry.hash);
        for profile_path in &profile_paths {
            let invoked = profile_path
                .join(".fingerprint")
                .join(&stem)
                .join("invoked.timestamp");
            if let Ok(meta) = fs::metadata(&invoked) {
                let ts = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                return (ts, RecencySource::FingerprintInvokedTimestamp);
            }
        }
    }
    (entry.mtime_unix, RecencySource::EntryMtime)
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

#[cfg(test)]
#[path = "prune_target_tests.rs"]
mod tests;
