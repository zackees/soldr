//! Root-local reclamation for `trash-*` buckets.
//!
//! The release-worktree CLI owns creation of these buckets.  Cleanup lives in
//! `soldr-cache` so the long-lived daemon can retry deletions without reaching
//! up into the CLI crate.  The supplied [`SoldrPaths`] root is the entire
//! ownership boundary; sibling product roots are never enumerated.

use crate::core::SoldrPaths;
use serde::Serialize;

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct SweepReport {
    pub removed: u64,
    pub retained: u64,
}

pub fn sweep_trash(paths: &SoldrPaths) -> std::io::Result<SweepReport> {
    let mut report = SweepReport::default();
    if !paths.root.exists() {
        return Ok(report);
    }
    crate::cache_lib::path_safety::validate_owned_directory(&paths.root, &paths.root)?;
    let entries = match std::fs::read_dir(&paths.root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(report),
        Err(error) => return Err(error),
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with("trash-") {
            continue;
        }
        let metadata = match std::fs::symlink_metadata(entry.path()) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if !metadata.is_dir() || crate::cache_lib::path_safety::is_link_or_reparse(&metadata) {
            continue;
        }
        crate::cache_lib::path_safety::validate_owned_directory(&paths.root, &entry.path())?;
        let bucket_entries = match std::fs::read_dir(entry.path()) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for child in bucket_entries.flatten() {
            let path = child.path();
            let result = match std::fs::symlink_metadata(&path) {
                Ok(kind) if crate::cache_lib::path_safety::is_link_or_reparse(&kind) => {
                    report.retained += 1;
                    continue;
                }
                Ok(kind) if kind.is_dir() => std::fs::remove_dir_all(&path),
                Ok(kind) if kind.is_file() => std::fs::remove_file(&path),
                _ => continue,
            };
            match result {
                Ok(()) => report.removed += 1,
                Err(_) => report.retained += 1,
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(sweep_is_exact_root_local_and_ignores_links, {
        let temp = tempfile::tempdir().unwrap();
        let owned = temp.path().join("owned");
        let sibling = temp.path().join("sibling");
        std::fs::create_dir_all(owned.join("trash-C/1")).unwrap();
        std::fs::create_dir_all(sibling.join("trash-C/keep")).unwrap();
        std::fs::write(owned.join("trash-C/1/file"), b"delete").unwrap();
        std::fs::write(sibling.join("trash-C/keep/sentinel"), b"keep").unwrap();

        let report = sweep_trash(&SoldrPaths::with_root(owned)).unwrap();
        assert_eq!(report.removed, 1);
        assert!(sibling.join("trash-C/keep/sentinel").is_file());
    });
}
