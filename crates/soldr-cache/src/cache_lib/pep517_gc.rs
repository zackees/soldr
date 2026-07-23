//! Retention for regenerable PEP 517 Cargo target namespaces.
//!
//! Targets live at `<soldr-root>/cargo-target/pep517/<project-id>`.  This
//! module deliberately accepts a [`SoldrPaths`] instead of discovering a home
//! directory, which keeps production, development, and custom roots isolated.

use crate::cache_lib::target_registry::directory_size;
use crate::core::SoldrPaths;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub const PRESSURE_MAX_AGE: Duration = Duration::from_secs(4 * 24 * 60 * 60);
pub const ABSOLUTE_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pep517GcCandidate {
    pub path: PathBuf,
    pub bytes: u64,
    pub age: Duration,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pep517GcReport {
    pub candidates: usize,
    pub removed: usize,
    pub retained: usize,
    pub failed: usize,
    pub bytes_reclaimed: u64,
}

pub fn target_root(paths: &SoldrPaths) -> PathBuf {
    paths.root.join("cargo-target").join("pep517")
}

pub fn wheel_root(paths: &SoldrPaths) -> PathBuf {
    paths.root.join("pep517").join("wheels")
}

pub fn scan(paths: &SoldrPaths, now: SystemTime, max_age: Duration) -> Vec<Pep517GcCandidate> {
    scan_root(paths, &target_root(paths), now, max_age).0
}

fn scan_root(
    paths: &SoldrPaths,
    root: &Path,
    now: SystemTime,
    max_age: Duration,
) -> (Vec<Pep517GcCandidate>, usize) {
    let root_meta = match std::fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return (Vec::new(), 0),
        Err(_) => return (Vec::new(), 1),
    };
    if !root_meta.is_dir()
        || crate::cache_lib::path_safety::is_link_or_reparse(&root_meta)
        || crate::cache_lib::path_safety::validate_owned_directory(&paths.root, root).is_err()
    {
        return (Vec::new(), 1);
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return (Vec::new(), 1);
    };
    let mut candidates = Vec::new();
    let mut failed = 0;
    for entry in entries {
        let Ok(entry) = entry else {
            failed += 1;
            continue;
        };
        let Ok(metadata) = std::fs::symlink_metadata(entry.path()) else {
            failed += 1;
            continue;
        };
        if !metadata.is_dir() {
            continue;
        }
        if crate::cache_lib::path_safety::is_link_or_reparse(&metadata) {
            failed += 1;
            continue;
        }
        let path = entry.path();
        let Ok(newest) = latest_tree_mtime(&path) else {
            failed += 1;
            continue;
        };
        let age = now.duration_since(newest).unwrap_or_default();
        if age >= max_age {
            candidates.push(Pep517GcCandidate {
                bytes: directory_size(&path),
                path,
                age,
            });
        }
    }
    candidates.sort_by(|left, right| {
        right
            .age
            .cmp(&left.age)
            .then_with(|| left.path.cmp(&right.path))
    });
    (candidates, failed)
}

pub fn sweep(paths: &SoldrPaths, now: SystemTime, max_age: Duration) -> Pep517GcReport {
    sweep_root(paths, &target_root(paths), now, max_age)
}

pub fn sweep_wheels(paths: &SoldrPaths, now: SystemTime, max_age: Duration) -> Pep517GcReport {
    sweep_root(paths, &wheel_root(paths), now, max_age)
}

fn sweep_root(
    paths: &SoldrPaths,
    root: &Path,
    now: SystemTime,
    max_age: Duration,
) -> Pep517GcReport {
    let (candidates, failed) = scan_root(paths, root, now, max_age);
    let mut report = Pep517GcReport {
        candidates: candidates.len(),
        retained: failed,
        failed,
        ..Pep517GcReport::default()
    };
    for candidate in candidates {
        match std::fs::remove_dir_all(&candidate.path) {
            Ok(()) => {
                report.removed += 1;
                report.bytes_reclaimed = report.bytes_reclaimed.saturating_add(candidate.bytes);
            }
            Err(_) => {
                report.retained += 1;
                report.failed += 1;
            }
        }
    }
    report
}

fn latest_tree_mtime(root: &Path) -> std::io::Result<SystemTime> {
    let metadata = std::fs::symlink_metadata(root)?;
    if crate::cache_lib::path_safety::is_link_or_reparse(&metadata) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("linked cache entry retained: {}", root.display()),
        ));
    }
    let mut latest = metadata.modified()?;
    if !metadata.is_dir() {
        return Ok(latest);
    }
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if crate::cache_lib::path_safety::is_link_or_reparse(&metadata) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("linked cache entry retained: {}", entry.path().display()),
            ));
        }
        latest = latest.max(if metadata.is_dir() {
            latest_tree_mtime(&entry.path())?
        } else {
            metadata.modified()?
        });
    }
    Ok(latest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use filetime::{set_file_mtime, FileTime};

    fn age_tree(path: &Path, when: SystemTime) {
        for entry in std::fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                age_tree(&entry.path(), when);
            }
            set_file_mtime(entry.path(), FileTime::from_system_time(when)).unwrap();
        }
        set_file_mtime(path, FileTime::from_system_time(when)).unwrap();
    }

    crate::timed_test!(pressure_and_absolute_boundaries_are_root_local, {
        let temp = tempfile::tempdir().unwrap();
        let owned = SoldrPaths::with_root(temp.path().join("owned"));
        let sibling = SoldrPaths::with_root(temp.path().join("sibling"));
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100 * 24 * 60 * 60);
        for (paths, name, age_days) in [
            (&owned, "old", 31_u64),
            (&owned, "pressure", 5),
            (&owned, "fresh", 1),
            (&sibling, "sentinel", 90),
        ] {
            let dir = target_root(paths).join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("artifact"), b"payload").unwrap();
            age_tree(&dir, now - Duration::from_secs(age_days * 24 * 60 * 60));
        }

        let full = sweep(&owned, now, ABSOLUTE_MAX_AGE);
        assert_eq!(full.removed, 1);
        assert!(target_root(&owned).join("pressure").exists());
        let pressure = sweep(&owned, now, PRESSURE_MAX_AGE);
        assert_eq!(pressure.removed, 1);
        assert!(target_root(&owned).join("fresh").exists());
        assert!(target_root(&sibling).join("sentinel/artifact").exists());
    });

    crate::timed_test!(wheel_cache_uses_the_same_root_local_age_policy, {
        let temp = tempfile::tempdir().unwrap();
        let owned = SoldrPaths::with_root(temp.path().join("owned"));
        let sibling = SoldrPaths::with_root(temp.path().join("sibling"));
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100 * 24 * 60 * 60);
        for (paths, name, age_days) in [
            (&owned, "old-project", 31_u64),
            (&owned, "fresh-project", 1),
            (&sibling, "sentinel", 90),
        ] {
            let dir = wheel_root(paths).join(name).join("wheel");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("artifact.whl"), b"payload").unwrap();
            age_tree(
                &wheel_root(paths).join(name),
                now - Duration::from_secs(age_days * 86_400),
            );
        }
        let report = sweep_wheels(&owned, now, ABSOLUTE_MAX_AGE);
        assert_eq!(report.removed, 1);
        assert!(wheel_root(&owned).join("fresh-project").is_dir());
        assert!(wheel_root(&sibling)
            .join("sentinel/wheel/artifact.whl")
            .is_file());
    });

    #[cfg(unix)]
    crate::timed_test!(
        timestamp_failure_retains_candidate_instead_of_failing_old,
        {
            let temp = tempfile::tempdir().unwrap();
            let paths = SoldrPaths::with_root(temp.path().join("owned"));
            let candidate = target_root(&paths).join("candidate");
            std::fs::create_dir_all(&candidate).unwrap();
            std::os::unix::fs::symlink(candidate.join("missing"), candidate.join("broken"))
                .unwrap();
            let report = sweep(&paths, SystemTime::now(), Duration::ZERO);
            assert_eq!(report.removed, 0);
            assert_eq!(report.failed, 1);
            assert!(candidate.is_dir());
        }
    );
}
