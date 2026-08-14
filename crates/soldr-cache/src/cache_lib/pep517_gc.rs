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

/// How many PEP 517 target namespaces to keep, newest first, regardless of
/// age.
///
/// Age alone does not bound this directory, because the failure mode is
/// *count*, not staleness. Every `pip install .` allocates a fresh
/// `<project-id>` namespace of roughly 2 GB and never reuses it, so a few
/// installs a day accumulate faster than a 30-day absolute age can reclaim,
/// and the 4-day pressure sweep only runs once free space is already below
/// `auto_gc.trigger_free_gb` (20 GB by default).
///
/// That gap is not theoretical: it filled a 1.8 TB volume to 46 GB free with
/// 23 namespaces spanning five days -- 47.9 GB of regenerable build output --
/// and the machine hard-hung during the next install, with no bugcheck and no
/// dump, when the pagefile could not grow on the full volume.
///
/// Three is enough to keep the current build plus a rebuild of the previous
/// revision warm, which is what makes reinstalls fast; older namespaces are
/// pure residue.
pub const RETAINED_TARGETS: usize = 3;

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

/// Namespaces beyond the newest [`RETAINED_TARGETS`], oldest first.
///
/// Independent of `max_age`: this is what bounds the directory when nothing
/// is old enough to qualify on age. Entries already selected by age are
/// excluded so a caller can concatenate the two without deleting twice.
fn over_retention_cap(
    paths: &SoldrPaths,
    root: &Path,
    now: SystemTime,
    already_selected: &[Pep517GcCandidate],
) -> Vec<Pep517GcCandidate> {
    // `max_age` of zero selects every namespace, which gives the full list
    // ordered oldest-first -- exactly the ranking the cap needs.
    let (all, _) = scan_root(paths, root, now, Duration::from_secs(0));
    if all.len() <= RETAINED_TARGETS {
        return Vec::new();
    }
    let selected: std::collections::HashSet<&Path> =
        already_selected.iter().map(|c| c.path.as_path()).collect();
    // `all` is oldest-first, so the newest `RETAINED_TARGETS` are at the tail.
    let cut = all.len() - RETAINED_TARGETS;
    all.into_iter()
        .take(cut)
        .filter(|c| !selected.contains(c.path.as_path()))
        .collect()
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
    let (age_selected, failed) = scan_root(paths, root, now, max_age);
    // Age first, then the count cap over whatever age did not already claim.
    // The cap is what bounds this directory in the common case: a namespace
    // is created per install and never reused, so on an active machine none
    // of them are old enough to qualify on age until the volume is already
    // in trouble.
    let over_cap = over_retention_cap(paths, root, now, &age_selected);
    let candidates: Vec<Pep517GcCandidate> = age_selected.into_iter().chain(over_cap).collect();
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

    #[test]
    fn pressure_and_absolute_boundaries_are_root_local() {
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
    }

    #[test]
    fn wheel_cache_uses_the_same_root_local_age_policy() {
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
    }

    #[test]
    fn timestamp_failure_retains_candidate_instead_of_failing_old() {
        let temp = tempfile::tempdir().unwrap();
        let paths = SoldrPaths::with_root(temp.path().join("owned"));
        let candidate = target_root(&paths).join("candidate");
        std::fs::create_dir_all(&candidate).unwrap();
        // A dangling symlink makes the timestamp probe fail. Skip on
        // hosts that cannot create one (Windows without Developer Mode).
        if crate::platform::fs::links::create(
            &candidate.join("missing").to_string_lossy(),
            &candidate.join("broken"),
            false,
        )
        .is_err()
        {
            return;
        }
        let report = sweep(&paths, SystemTime::now(), Duration::ZERO);
        assert_eq!(report.removed, 0);
        assert_eq!(report.failed, 1);
        assert!(candidate.is_dir());
    }
}

#[cfg(test)]
mod retention_cap_tests {
    use super::*;
    use std::time::Duration;

    fn ns(root: &Path, name: &str, age_secs: u64) -> PathBuf {
        let dir = root.join("cargo-target").join("pep517").join(name);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("blob.bin"), b"x").expect("write");
        let when = std::time::SystemTime::now() - Duration::from_secs(age_secs);
        let stamp = filetime::FileTime::from_system_time(when);
        // Both the file *and* the directory: `latest_tree_mtime` takes the
        // newest mtime in the tree, so a freshly created directory keeps the
        // namespace looking new no matter how old its contents are. Aging
        // only the file makes every age-based assertion vacuous.
        filetime::set_file_mtime(dir.join("blob.bin"), stamp).expect("set file mtime");
        filetime::set_file_mtime(&dir, stamp).expect("set dir mtime");
        dir
    }

    // The reported failure: 23 namespaces spanning five days, none old
    // enough for the 30-day absolute sweep, and the 4-day pressure sweep
    // never reached because free space was still above the trigger. The cap
    // is what bounds this.
    #[test]
    fn namespaces_beyond_the_cap_are_reclaimed_regardless_of_age() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());
        // All young: an age-only sweep would keep every one of them.
        for i in 0..8u64 {
            ns(temp.path(), &format!("ns{i:02}"), 3600 * (8 - i));
        }

        let report = sweep(&paths, SystemTime::now(), ABSOLUTE_MAX_AGE);
        assert_eq!(
            report.removed,
            8 - RETAINED_TARGETS,
            "everything past the newest {RETAINED_TARGETS} should go: {report:?}",
        );
        let left = std::fs::read_dir(target_root(&paths))
            .expect("read")
            .count();
        assert_eq!(left, RETAINED_TARGETS);
    }

    #[test]
    fn at_or_under_the_cap_nothing_is_removed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());
        for i in 0..RETAINED_TARGETS as u64 {
            ns(temp.path(), &format!("ns{i:02}"), 3600 * (i + 1));
        }

        let report = sweep(&paths, SystemTime::now(), ABSOLUTE_MAX_AGE);
        assert_eq!(report.removed, 0, "{report:?}");
    }

    // The newest namespace is the one a reinstall reuses; reclaiming it
    // would make every install cold.
    #[test]
    fn the_newest_namespace_always_survives() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());
        for i in 0..6u64 {
            ns(temp.path(), &format!("ns{i:02}"), 3600 * (6 - i));
        }
        let newest = target_root(&paths).join("ns05");

        sweep(&paths, SystemTime::now(), ABSOLUTE_MAX_AGE);
        assert!(newest.exists(), "the newest namespace must survive");
    }

    // Age and cap must not both claim the same directory and double-count.
    #[test]
    fn age_and_cap_do_not_double_count() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());
        for i in 0..6u64 {
            // All far older than the max age passed below.
            ns(temp.path(), &format!("ns{i:02}"), 40 * 24 * 3600 + i);
        }

        let report = sweep(&paths, SystemTime::now(), ABSOLUTE_MAX_AGE);
        assert_eq!(
            report.removed, 6,
            "all are past the absolute age: {report:?}"
        );
        assert_eq!(
            report.failed, 0,
            "a double-claim shows up as a failed delete"
        );
    }
}
