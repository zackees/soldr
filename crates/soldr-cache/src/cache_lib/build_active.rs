//! Build activity tracking for cleanup coordination.
//!
//! The atomic flag is retained for cheap same-process checks, but the
//! cross-process authority is an OS-held lease file. A detached GC process can
//! therefore see a build owned by a different soldr process, and overlapping
//! sessions cannot clear one another's activity.

use crate::core::SoldrPaths;
use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

static IS_BUILD_ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn set(active: bool) {
    IS_BUILD_ACTIVE.store(active, Ordering::Release);
}

pub fn is_active() -> bool {
    IS_BUILD_ACTIVE.load(Ordering::Acquire)
}

fn lease_dir(paths: &SoldrPaths) -> PathBuf {
    paths.root.join("build-leases")
}

/// An OS-held lease that marks one build session active across processes.
pub struct BuildActivityLease {
    file: File,
    path: PathBuf,
}

impl BuildActivityLease {
    pub fn acquire(paths: &SoldrPaths, session_id: u64) -> io::Result<Self> {
        let dir = lease_dir(paths);
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{session_id:016x}.lock"));
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)?;
        file.try_lock_exclusive()?;
        Ok(Self { file, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for BuildActivityLease {
    fn drop(&mut self) {
        let _ = self.file.unlock();
        let _ = fs::remove_file(&self.path);
    }
}

/// Return whether any other process currently holds a build lease.
///
/// Unreadable state fails closed as active. Stale, unlocked lease files are
/// removed after successfully acquiring their lock.
pub fn any_active(paths: &SoldrPaths) -> io::Result<bool> {
    let dir = lease_dir(paths);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_file() {
            continue;
        }
        let path = entry.path();
        let file = OpenOptions::new().read(true).write(true).open(&path)?;
        match file.try_lock_exclusive() {
            Ok(()) => {
                let _ = file.unlock();
                let _ = fs::remove_file(path);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(true),
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(set_clear_round_trip, {
        set(true);
        assert!(is_active());
        set(false);
        assert!(!is_active());
    });

    crate::timed_test!(leases_coordinate_and_cleanup_stale_files, {
        let temp = tempfile::tempdir().unwrap();
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());
        let first = BuildActivityLease::acquire(&paths, 1).unwrap();
        let second = BuildActivityLease::acquire(&paths, 2).unwrap();
        assert!(any_active(&paths).unwrap());
        drop(first);
        assert!(any_active(&paths).unwrap());
        drop(second);
        assert!(!any_active(&paths).unwrap());
    });
}
