//! Redb-backed registry of observed Cargo `target/` directories.
//!
//! Implements the data plane for issue #234: every `RUSTC_WRAPPER`
//! invocation upserts the resolved workspace `target/` path with the
//! current unix timestamp. The `soldr gc` command later scans the
//! registry to find stale candidates for reclamation.
//!
//! The store lives in `~/.soldr/state.redb` alongside other soldr state.

use crate::cache_lib::redb_lock::{open_state_db, StateDbHandle};
use redb::{
    backends::InMemoryBackend, Database, ReadableDatabase, ReadableTable, ReadableTableMetadata,
    TableDefinition,
};
use std::{
    ops::Deref,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

/// Default file name for the soldr state database under `~/.soldr/`.
pub const DATA_DB_FILE: &str = super::state_db::STATE_DB_FILE;

/// Default staleness threshold (10 days) used by `soldr gc` and the
/// startup warning.
pub const DEFAULT_STALE_AGE_SECONDS: u64 = 10 * 24 * 60 * 60;

/// Default size threshold (256 MiB) used by `soldr gc` and the
/// startup warning.
pub const DEFAULT_STALE_SIZE_BYTES: u64 = 256 * 1024 * 1024;

const TARGETS: TableDefinition<&str, i64> = TableDefinition::new("target_registry_targets");

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("redb database error: {0}")]
    Database(#[from] redb::DatabaseError),
    #[error("redb transaction error: {0}")]
    Transaction(#[from] redb::TransactionError),
    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),
    #[error("redb commit error: {0}")]
    Commit(#[from] redb::CommitError),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("system clock before unix epoch: {0}")]
    Clock(String),
}

/// One row from the `targets` table augmented with the current
/// observation time so callers can compute the age cheaply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetRow {
    pub path: PathBuf,
    pub last_used: i64,
}

/// Internal backing store for [`TargetRegistry`]. File-backed instances
/// hold the process-wide [`state_db_open_lock`] guard alongside the
/// redb [`Database`] so concurrent opens against `state.redb` from
/// other daemon code paths (`daemon::db`, `cache_lib::cook_index`)
/// serialize safely (#608). In-memory instances bypass the lock —
/// each is an independent database with no file backing and no
/// cross-instance contention.
enum TargetRegistryDb {
    File(StateDbHandle),
    InMemory(Database),
}

impl Deref for TargetRegistryDb {
    type Target = Database;
    fn deref(&self) -> &Database {
        match self {
            Self::File(h) => h,
            Self::InMemory(db) => db,
        }
    }
}

/// Redb registry backed by `~/.soldr/state.redb` (or a caller-provided
/// path).
pub struct TargetRegistry {
    db: TargetRegistryDb,
}

impl TargetRegistry {
    /// Open or create the database at the given path. Parent dirs are
    /// created automatically. The returned registry holds the
    /// process-wide [`state_db_open_lock`] guard for its entire
    /// lifetime so the redb file lock is never contended by another
    /// in-process opener — the `RecordTargetTouch` handler runs on
    /// every rustc-wrapper call and would otherwise race with
    /// `daemon::db` / `cache_lib::cook_index` (#608).
    pub fn open(path: &Path) -> Result<Self, RegistryError> {
        let handle = open_state_db(path)?;
        Self::init_schema(&handle)?;
        Ok(Self {
            db: TargetRegistryDb::File(handle),
        })
    }

    /// Open an in-memory database. Useful for tests and for callers
    /// that want a registry without touching disk. In-memory instances
    /// are independent databases (no file backing), so they do not
    /// acquire [`state_db_open_lock`].
    pub fn open_in_memory() -> Result<Self, RegistryError> {
        let db = Database::builder().create_with_backend(InMemoryBackend::new())?;
        Self::init_schema(&db)?;
        Ok(Self {
            db: TargetRegistryDb::InMemory(db),
        })
    }

    fn init_schema(db: &Database) -> Result<(), RegistryError> {
        let write_txn = db.begin_write()?;
        {
            let _table = write_txn.open_table(TARGETS)?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Insert or update the row for `path` with the supplied unix
    /// timestamp.
    pub fn upsert_with_time(&self, path: &Path, unix_seconds: i64) -> Result<(), RegistryError> {
        let path_str = path_to_string(path);
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(TARGETS)?;
            table.insert(path_str.as_str(), &unix_seconds)?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Convenience wrapper that stamps the row with the current time.
    pub fn upsert(&self, path: &Path) -> Result<(), RegistryError> {
        self.upsert_with_time(path, current_unix_seconds()?)
    }

    /// Return all tracked rows, ordered by `last_used` ascending.
    pub fn list(&self) -> Result<Vec<TargetRow>, RegistryError> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(TARGETS)?;
        let mut rows = Vec::new();
        for row in table.iter()? {
            let (path, last_used) = row?;
            rows.push(TargetRow {
                path: PathBuf::from(path.value()),
                last_used: last_used.value(),
            });
        }
        rows.sort_by(|a, b| {
            a.last_used
                .cmp(&b.last_used)
                .then_with(|| a.path.cmp(&b.path))
        });
        Ok(rows)
    }

    /// Look up a single tracked row by path, if present.
    pub fn get(&self, path: &Path) -> Result<Option<TargetRow>, RegistryError> {
        let path_str = path_to_string(path);
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(TARGETS)?;
        let Some(last_used) = table.get(path_str.as_str())? else {
            return Ok(None);
        };
        Ok(Some(TargetRow {
            path: PathBuf::from(path_str),
            last_used: last_used.value(),
        }))
    }

    /// Remove the row for `path`. No-op if it doesn't exist.
    pub fn remove(&self, path: &Path) -> Result<bool, RegistryError> {
        let path_str = path_to_string(path);
        let write_txn = self.db.begin_write()?;
        let removed = {
            let mut table = write_txn.open_table(TARGETS)?;
            let old = table.remove(path_str.as_str())?;
            old.is_some()
        };
        write_txn.commit()?;
        Ok(removed)
    }

    /// Remove rows for every path in `paths` in a single write
    /// transaction. Missing rows are skipped. Returns the number of
    /// rows that were actually removed.
    pub fn remove_many(&self, paths: &[PathBuf]) -> Result<usize, RegistryError> {
        if paths.is_empty() {
            return Ok(0);
        }
        let write_txn = self.db.begin_write()?;
        let mut removed = 0usize;
        {
            let mut table = write_txn.open_table(TARGETS)?;
            for path in paths {
                let key = path_to_string(path);
                if table.remove(key.as_str())?.is_some() {
                    removed += 1;
                }
            }
        }
        write_txn.commit()?;
        Ok(removed)
    }

    /// Total number of tracked rows.
    pub fn len(&self) -> Result<usize, RegistryError> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(TARGETS)?;
        Ok(table.len()? as usize)
    }

    /// Whether the registry has any rows.
    pub fn is_empty(&self) -> Result<bool, RegistryError> {
        Ok(self.len()? == 0)
    }
}

/// Human-readable byte size, kibibyte-based (e.g. `12.3 GB`).
pub fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB", "PB"];
    if bytes == 0 {
        return "0 B".to_string();
    }
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{:.1} {}", size, UNITS[unit])
    }
}

/// Human-readable age in days/hours from a delta of seconds.
pub fn human_age(seconds: i64) -> String {
    if seconds < 0 {
        return "future".to_string();
    }
    let secs = seconds as u64;
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    if days > 0 {
        format!("{days}d{hours}h")
    } else {
        let minutes = (secs % 3_600) / 60;
        format!("{hours}h{minutes}m")
    }
}

/// Recursively measure the on-disk size of a directory in bytes,
/// following directory entries but never crossing symlinks. Errors are
/// silently swallowed for individual entries — partial sizes are still
/// useful for the GC heuristic.
pub fn directory_size(path: &Path) -> u64 {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    if metadata.file_type().is_symlink() {
        return 0;
    }
    if metadata.is_file() {
        return metadata.len();
    }
    let mut total: u64 = 0;
    let entries = match std::fs::read_dir(path) {
        Ok(e) => e,
        Err(_) => return 0,
    };
    for entry in entries.flatten() {
        let entry_path = entry.path();
        let entry_meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if entry_meta.file_type().is_symlink() {
            continue;
        }
        if entry_meta.is_dir() {
            total = total.saturating_add(directory_size(&entry_path));
        } else if entry_meta.is_file() {
            total = total.saturating_add(entry_meta.len());
        }
    }
    total
}

/// Recursively measure both on-disk size (bytes) and file count for
/// a directory in a single walk. Same symlink/error semantics as
/// [`directory_size`]: symlinks are not followed, individual entry
/// errors are swallowed. Returns `(total_bytes, file_count)`.
pub fn directory_size_and_files(path: &Path) -> (u64, u64) {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return (0, 0),
    };
    if metadata.file_type().is_symlink() {
        return (0, 0);
    }
    if metadata.is_file() {
        return (metadata.len(), 1);
    }
    let mut total_bytes: u64 = 0;
    let mut total_files: u64 = 0;
    let entries = match std::fs::read_dir(path) {
        Ok(e) => e,
        Err(_) => return (0, 0),
    };
    for entry in entries.flatten() {
        let entry_path = entry.path();
        let entry_meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if entry_meta.file_type().is_symlink() {
            continue;
        }
        if entry_meta.is_dir() {
            let (sub_bytes, sub_files) = directory_size_and_files(&entry_path);
            total_bytes = total_bytes.saturating_add(sub_bytes);
            total_files = total_files.saturating_add(sub_files);
        } else if entry_meta.is_file() {
            total_bytes = total_bytes.saturating_add(entry_meta.len());
            total_files = total_files.saturating_add(1);
        }
    }
    (total_bytes, total_files)
}

/// Resolve the workspace `target/` dir from an arbitrary path that
/// sits *inside* a `target/` tree. Walks up until it finds a path
/// component named `target`. Returns `None` if no such ancestor is
/// found cheaply — we never walk the whole filesystem.
pub fn resolve_target_dir_from_descendant(start: &Path) -> Option<PathBuf> {
    let mut current = start;
    let mut last_target: Option<PathBuf> = None;
    while let Some(parent) = current.parent() {
        if parent.file_name().map(|n| n == "target").unwrap_or(false) {
            last_target = Some(parent.to_path_buf());
        }
        current = parent;
    }
    last_target
}

/// Resolve the canonical workspace `target/` directory from a
/// rustc-wrapper argv slice. Returns `None` if the path can't be
/// derived cheaply — callers MUST silently skip in that case rather
/// than fail the build.
///
/// Strategy:
/// 1. Honor `CARGO_TARGET_DIR` if set and absolute.
/// 2. Otherwise look at the rustc `--out-dir <DIR>` argument and walk
///    up to find the enclosing `target/` boundary.
pub fn resolve_workspace_target_dir(rustc_args: &[String]) -> Option<PathBuf> {
    if let Some(env_dir) = std::env::var_os("CARGO_TARGET_DIR") {
        let path = PathBuf::from(env_dir);
        if path.is_absolute() {
            return Some(canonicalize_or_self(&path));
        }
    }

    let out_dir = extract_flag_value(rustc_args, "--out-dir")?;
    let out_path = PathBuf::from(out_dir);
    let target = resolve_target_dir_from_descendant(&out_path)?;
    Some(canonicalize_or_self(&target))
}

fn extract_flag_value(args: &[String], flag: &str) -> Option<String> {
    let mut iter = args.iter();
    let prefix = format!("{flag}=");
    while let Some(arg) = iter.next() {
        if arg == flag {
            return iter.next().cloned();
        }
        if let Some(rest) = arg.strip_prefix(&prefix) {
            return Some(rest.to_string());
        }
    }
    None
}

fn canonicalize_or_self(path: &Path) -> PathBuf {
    match std::fs::canonicalize(path) {
        Ok(canonical) => canonical,
        Err(_) => path.to_path_buf(),
    }
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

/// Current unix timestamp in seconds.
pub fn current_unix_seconds() -> Result<i64, RegistryError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .map_err(|e| RegistryError::Clock(e.to_string()))
}

// ---------------------------------------------------------------------------
// Safety guards (from docs/TARGET_GC_PROPOSAL.md)
// ---------------------------------------------------------------------------

/// Decision returned by [`evaluate_safety_guards`] for a single
/// candidate `target/` directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardOutcome {
    /// Safe to consider for GC.
    Eligible,
    /// Skipped because of an active build lock or recent activity.
    Skipped(String),
}

/// Apply the proposal's three safety guards to a candidate `target/`
/// directory:
///
/// (a) `Cargo.lock` mtime within the staleness window — workspace was
///     built recently so the dir is likely live.
/// (b) `target/.cargo-lock` exists — cargo is currently holding the
///     build lock.
/// (c) `dev_root` allowlist — the `target/` path must sit under one
///     of the configured roots (defaults to `~/dev`).
pub fn evaluate_safety_guards(
    target_dir: &Path,
    workspace_root: &Path,
    dev_roots: &[PathBuf],
    stale_age_seconds: u64,
    now_unix_seconds: i64,
) -> GuardOutcome {
    if !is_under_any_root(target_dir, dev_roots) {
        return GuardOutcome::Skipped(format!(
            "outside configured gc.allowlist_roots ({} roots)",
            dev_roots.len()
        ));
    }

    let cargo_lock = workspace_root.join("Cargo.lock");
    if let Ok(meta) = std::fs::metadata(&cargo_lock) {
        if let Ok(modified) = meta.modified() {
            if let Ok(elapsed) = modified
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
            {
                let age = now_unix_seconds.saturating_sub(elapsed);
                if (age as u64) < stale_age_seconds {
                    return GuardOutcome::Skipped(format!(
                        "Cargo.lock modified {} ago (< staleness threshold)",
                        human_age(age)
                    ));
                }
            }
        }
    }

    match super::cargo_lock::probe(target_dir) {
        Ok(super::cargo_lock::CargoLockProbe::Idle(_guard)) => {}
        Ok(super::cargo_lock::CargoLockProbe::Active(lock)) => {
            return GuardOutcome::Skipped(format!(
                "active cargo build lock present at {}",
                lock.display()
            ));
        }
        Err(error) => {
            return GuardOutcome::Skipped(format!(
                "cargo build lock probe failed closed: {error}"
            ));
        }
    }

    GuardOutcome::Eligible
}

/// Whether `path` lies under any of the supplied `dev_roots` after
/// canonicalization. An empty `dev_roots` slice rejects everything.
pub fn is_under_any_root(path: &Path, dev_roots: &[PathBuf]) -> bool {
    if dev_roots.is_empty() {
        return false;
    }
    let canonical = canonicalize_or_self(path);
    for root in dev_roots {
        let root_canonical = canonicalize_or_self(root);
        if canonical.starts_with(&root_canonical) {
            return true;
        }
    }
    false
}

/// Walk up from a `target/` dir to its workspace root. The workspace
/// root is the parent directory of `target/` (the one containing
/// `Cargo.toml`).
pub fn workspace_root_for_target(target_dir: &Path) -> PathBuf {
    target_dir
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| target_dir.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs2::FileExt;
    use tempfile::tempdir;

    fn fixed_now() -> i64 {
        1_700_000_000
    }

    #[test]
    fn upsert_is_idempotent_and_updates_timestamp() {
        let registry = TargetRegistry::open_in_memory().unwrap();
        let path = PathBuf::from("/tmp/repo/target");
        registry.upsert_with_time(&path, 100).unwrap();
        registry.upsert_with_time(&path, 200).unwrap();
        registry.upsert_with_time(&path, 200).unwrap();

        let row = registry.get(&path).unwrap().unwrap();
        assert_eq!(row.last_used, 200);
        assert_eq!(registry.len().unwrap(), 1);
    }

    #[test]
    fn list_returns_all_rows_sorted() {
        let registry = TargetRegistry::open_in_memory().unwrap();
        registry
            .upsert_with_time(Path::new("/a/target"), 300)
            .unwrap();
        registry
            .upsert_with_time(Path::new("/b/target"), 100)
            .unwrap();
        registry
            .upsert_with_time(Path::new("/c/target"), 200)
            .unwrap();

        let rows = registry.list().unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].path, PathBuf::from("/b/target"));
        assert_eq!(rows[1].path, PathBuf::from("/c/target"));
        assert_eq!(rows[2].path, PathBuf::from("/a/target"));
    }

    #[test]
    fn remove_deletes_row() {
        let registry = TargetRegistry::open_in_memory().unwrap();
        let path = PathBuf::from("/tmp/repo/target");
        registry.upsert_with_time(&path, 100).unwrap();
        assert!(registry.remove(&path).unwrap());
        assert!(!registry.remove(&path).unwrap());
        assert_eq!(registry.len().unwrap(), 0);
    }

    #[test]
    fn remove_many_batches_deletes_and_counts_hits() {
        let registry = TargetRegistry::open_in_memory().unwrap();
        let a = PathBuf::from("/tmp/a/target");
        let b = PathBuf::from("/tmp/b/target");
        let c = PathBuf::from("/tmp/c/target");
        let ghost = PathBuf::from("/tmp/ghost/target");
        registry.upsert_with_time(&a, 10).unwrap();
        registry.upsert_with_time(&b, 20).unwrap();
        registry.upsert_with_time(&c, 30).unwrap();

        let removed = registry
            .remove_many(&[a.clone(), ghost.clone(), b.clone()])
            .unwrap();
        assert_eq!(removed, 2);
        assert!(registry.get(&a).unwrap().is_none());
        assert!(registry.get(&b).unwrap().is_none());
        assert!(registry.get(&c).unwrap().is_some());
        assert!(registry.get(&ghost).unwrap().is_none());
    }

    #[test]
    fn remove_many_on_empty_input_is_a_noop() {
        let registry = TargetRegistry::open_in_memory().unwrap();
        let path = PathBuf::from("/tmp/keep/target");
        registry.upsert_with_time(&path, 5).unwrap();
        assert_eq!(registry.remove_many(&[]).unwrap(), 0);
        assert!(registry.get(&path).unwrap().is_some());
    }

    #[test]
    fn open_persists_to_disk() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("state.redb");
        {
            let registry = TargetRegistry::open(&db_path).unwrap();
            registry
                .upsert_with_time(Path::new("/tmp/persisted/target"), 555)
                .unwrap();
        }
        let registry = TargetRegistry::open(&db_path).unwrap();
        let row = registry
            .get(Path::new("/tmp/persisted/target"))
            .unwrap()
            .unwrap();
        assert_eq!(row.last_used, 555);
    }

    #[test]
    fn open_in_memory_uses_isolated_redb_backend() {
        let first = TargetRegistry::open_in_memory().unwrap();
        let second = TargetRegistry::open_in_memory().unwrap();
        first
            .upsert_with_time(Path::new("/tmp/first/target"), 10)
            .unwrap();

        assert_eq!(first.len().unwrap(), 1);
        assert!(second.is_empty().unwrap());
    }

    #[test]
    fn human_size_formats_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1024 * 1024), "1.0 MB");
        assert_eq!(human_size(2u64 * 1024 * 1024 * 1024), "2.0 GB");
    }

    #[test]
    fn human_age_formats_days_and_hours() {
        assert_eq!(human_age(0), "0h0m");
        assert_eq!(human_age(3600), "1h0m");
        assert_eq!(human_age(86_400), "1d0h");
        assert_eq!(human_age(86_400 * 3 + 3600 * 4), "3d4h");
    }

    #[test]
    fn directory_size_sums_files() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        std::fs::write(dir.path().join("b.bin"), vec![0u8; 1024]).unwrap();
        let nested = dir.path().join("sub");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("c.bin"), vec![0u8; 256]).unwrap();
        assert_eq!(directory_size(dir.path()), 5 + 1024 + 256);
    }

    #[test]
    fn resolve_target_dir_walks_up_to_target_boundary() {
        let path = PathBuf::from("/home/user/repo/target/debug/deps/foo-abcdef");
        let target = resolve_target_dir_from_descendant(&path).unwrap();
        assert_eq!(target, PathBuf::from("/home/user/repo/target"));
    }

    #[test]
    fn resolve_target_dir_returns_none_when_no_target_ancestor() {
        let path = PathBuf::from("/home/user/some/other/dir");
        assert!(resolve_target_dir_from_descendant(&path).is_none());
    }

    #[test]
    fn extract_flag_value_handles_space_and_equals_forms() {
        let args: Vec<String> = vec![
            "--crate-name".into(),
            "foo".into(),
            "--out-dir".into(),
            "/tmp/repo/target/debug/deps".into(),
        ];
        assert_eq!(
            extract_flag_value(&args, "--out-dir"),
            Some("/tmp/repo/target/debug/deps".into())
        );

        let args2: Vec<String> = vec!["--out-dir=/tmp/repo/target/debug/deps".into()];
        assert_eq!(
            extract_flag_value(&args2, "--out-dir"),
            Some("/tmp/repo/target/debug/deps".into())
        );
    }

    #[test]
    fn allowlist_guard_short_circuits_outside_dev_roots() {
        let dir = tempdir().unwrap();
        let target_dir = dir.path().join("repo").join("target");
        std::fs::create_dir_all(&target_dir).unwrap();
        let workspace = target_dir.parent().unwrap().to_path_buf();
        // No dev_roots configured => everything is rejected.
        let outcome = evaluate_safety_guards(
            &target_dir,
            &workspace,
            &[],
            DEFAULT_STALE_AGE_SECONDS,
            fixed_now(),
        );
        assert!(matches!(outcome, GuardOutcome::Skipped(_)));
    }

    #[test]
    fn allowlist_guard_passes_when_target_is_under_root() {
        let dir = tempdir().unwrap();
        let target_dir = dir.path().join("repo").join("target");
        std::fs::create_dir_all(&target_dir).unwrap();
        let workspace = target_dir.parent().unwrap().to_path_buf();

        let outcome = evaluate_safety_guards(
            &target_dir,
            &workspace,
            &[dir.path().to_path_buf()],
            DEFAULT_STALE_AGE_SECONDS,
            fixed_now(),
        );
        assert_eq!(outcome, GuardOutcome::Eligible);
    }

    #[test]
    fn cargo_build_lock_skips_candidate() {
        let dir = tempdir().unwrap();
        let target_dir = dir.path().join("repo").join("target");
        std::fs::create_dir_all(&target_dir).unwrap();
        let workspace = target_dir.parent().unwrap().to_path_buf();
        let cargo_lock = std::fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(target_dir.join(".cargo-lock"))
            .unwrap();
        cargo_lock.try_lock_exclusive().unwrap();

        let outcome = evaluate_safety_guards(
            &target_dir,
            &workspace,
            &[dir.path().to_path_buf()],
            DEFAULT_STALE_AGE_SECONDS,
            fixed_now(),
        );
        match outcome {
            GuardOutcome::Skipped(reason) => assert!(reason.contains("cargo build lock")),
            _ => panic!("expected skip"),
        }
    }

    #[test]
    fn fresh_cargo_lock_skips_candidate() {
        let dir = tempdir().unwrap();
        let target_dir = dir.path().join("repo").join("target");
        std::fs::create_dir_all(&target_dir).unwrap();
        let workspace = target_dir.parent().unwrap().to_path_buf();
        // Cargo.lock that is mtime "now" — newer than the threshold.
        std::fs::write(workspace.join("Cargo.lock"), b"").unwrap();

        let outcome = evaluate_safety_guards(
            &target_dir,
            &workspace,
            &[dir.path().to_path_buf()],
            DEFAULT_STALE_AGE_SECONDS,
            current_unix_seconds().unwrap(),
        );
        match outcome {
            GuardOutcome::Skipped(reason) => assert!(reason.contains("Cargo.lock modified")),
            _ => panic!("expected skip"),
        }
    }

    #[test]
    fn workspace_root_is_target_parent() {
        let target = PathBuf::from("/home/me/repo/target");
        assert_eq!(
            workspace_root_for_target(&target),
            PathBuf::from("/home/me/repo")
        );
    }
}
