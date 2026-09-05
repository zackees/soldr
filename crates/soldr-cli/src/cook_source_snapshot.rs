//! Project-source snapshot/restore used by `soldr cook` to undo
//! cargo-chef's in-place skeleton reconstruction (zackees/soldr#566).
//!
//! Split out of `cook.rs` so that file stays under the 1,000-line
//! production ceiling enforced by `.github/scripts/loc_ceiling.py`.

use crate::core::SoldrError;
use std::path::{Path, PathBuf};

/// In-memory snapshot of a project's source-defining files (every
/// `Cargo.toml` / `Cargo.lock` / `*.rs` outside `target/` and `.git/`), used
/// to undo cargo-chef's in-place skeleton reconstruction after the cook
/// compile so `soldr cook` leaves the project pristine (zackees/soldr#566).
pub(crate) struct ProjectSourceSnapshot {
    pub(crate) files: Vec<(PathBuf, Vec<u8>)>,
}

impl ProjectSourceSnapshot {
    /// File count captured (exposed for tests/diagnostics).
    #[allow(clippy::len_without_is_empty)]
    pub(crate) fn len(&self) -> usize {
        self.files.len()
    }
}

/// True for the files cargo-chef rewrites in place: crate manifests, the
/// lockfile, and Rust sources (crate roots get stubbed). Restricting the
/// snapshot to these keeps it small (source, not build output).
fn is_project_source_file(name: &str) -> bool {
    name == "Cargo.toml" || name == "Cargo.lock" || name.ends_with(".rs")
}

/// Recurse `dir`, skipping `target/` and `.git/` at any depth, invoking `f`
/// on every regular project-source file with its path relative to `base`.
fn walk_project_source(
    dir: &Path,
    base: &Path,
    f: &mut dyn FnMut(&Path, PathBuf),
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let path = entry.path();
        if file_type.is_dir() {
            if name.as_ref() == "target" || name.as_ref() == ".git" {
                continue;
            }
            walk_project_source(&path, base, f)?;
        } else if file_type.is_file() && is_project_source_file(name.as_ref()) {
            let rel = path
                .strip_prefix(base)
                .unwrap_or(path.as_path())
                .to_path_buf();
            f(path.as_path(), rel);
        }
    }
    Ok(())
}

/// Capture the project's source-defining files under `manifest_dir`.
pub(crate) fn snapshot_project_source(
    manifest_dir: &Path,
) -> Result<ProjectSourceSnapshot, SoldrError> {
    let mut files: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    {
        let mut collect = |abs: &Path, rel: PathBuf| {
            if let Ok(bytes) = std::fs::read(abs) {
                files.push((rel, bytes));
            }
        };
        walk_project_source(manifest_dir, manifest_dir, &mut collect).map_err(|e| {
            SoldrError::Other(format!(
                "soldr cook: failed to snapshot project source under {}: {e}",
                manifest_dir.display()
            ))
        })?;
    }
    Ok(ProjectSourceSnapshot { files })
}

/// Restore the project to its snapshotted state: delete every current
/// project-source file that the snapshot does not contain (removing any
/// crate roots cargo-chef added for crates that previously had none), then
/// rewrite the captured originals. Files whose on-disk contents already
/// match the snapshot are left untouched so their mtimes survive —
/// zackees/soldr#3043: cook runs inside CI jobs after earlier build steps,
/// and rewriting every file on every restore bumps mtimes and invalidates
/// Cargo's fingerprints for work that already completed in the same job.
/// `target/`/`.git/` are never touched, so the cooked dependency artifacts
/// survive.
pub(crate) fn restore_project_source(
    manifest_dir: &Path,
    snapshot: &ProjectSourceSnapshot,
) -> Result<(), SoldrError> {
    // 1. Remove files cargo-chef added that the snapshot never captured
    // (manifests + .rs outside target/.git). Files present in the snapshot
    // are handled below by content comparison instead of blind delete+write.
    let known: std::collections::HashSet<PathBuf> =
        snapshot.files.iter().map(|(rel, _)| rel.clone()).collect();
    let mut to_delete: Vec<PathBuf> = Vec::new();
    {
        let mut mark = |abs: &Path, rel: PathBuf| {
            if !known.contains(&rel) {
                to_delete.push(abs.to_path_buf());
            }
        };
        // Best-effort: a read error here just means fewer deletions; the
        // rewrite below still restores originals.
        let _ = walk_project_source(manifest_dir, manifest_dir, &mut mark);
    }
    for path in to_delete {
        let _ = std::fs::remove_file(&path);
    }
    // 2. Rewrite the captured originals, skipping files whose content is
    // already correct so their mtime is preserved.
    for (rel, bytes) in &snapshot.files {
        let dest = manifest_dir.join(rel);
        // Already byte-identical: leave the file (and therefore its mtime)
        // alone. A read error — missing file, unreadable, or a directory
        // standing where a file belongs — falls through to the write below so
        // the failure is still reported by the original error path.
        if std::fs::read(&dest).is_ok_and(|current| current == *bytes) {
            continue;
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                SoldrError::Other(format!(
                    "soldr cook: failed to restore directory {}: {e}",
                    parent.display()
                ))
            })?;
        }
        std::fs::write(&dest, bytes).map_err(|e| {
            SoldrError::Other(format!(
                "soldr cook: failed to restore {}: {e}",
                dest.display()
            ))
        })?;
    }
    Ok(())
}
