//! Real Cargo lock probing for target cleanup.

use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

pub enum CargoLockProbe {
    Idle(CargoLockGuard),
    Active(PathBuf),
}

/// File handles whose exclusive locks remain held until the protected
/// operation completes.
pub struct CargoLockGuard {
    files: Vec<File>,
}

/// Whether an error means another process already holds the file lock.
///
/// Windows can reject the second open itself with ERROR_SHARING_VIOLATION
/// (32) or ERROR_LOCK_VIOLATION (33), rather than letting fs2 return
/// WouldBlock from try_lock_exclusive as Unix does.
pub fn lock_is_held(error: &io::Error) -> bool {
    crate::platform::fs::contention::is_lock_contention(error)
}

impl Drop for CargoLockGuard {
    fn drop(&mut self) {
        for file in &self.files {
            let _ = file.unlock();
        }
    }
}

/// Recursively discover Cargo's .cargo-lock sentinels and try to acquire all
/// of them. Existence alone is not activity: Cargo leaves these files behind
/// after builds. An unreadable directory or lock probe error is returned so
/// callers can fail closed.
pub fn probe(target_dir: &Path) -> io::Result<CargoLockProbe> {
    let mut lock_paths = Vec::new();
    let mut pending = vec![target_dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries = fs::read_dir(&dir)?;
        for entry in entries {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let path = entry.path();
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                pending.push(path);
            } else if file_type.is_file()
                && path.file_name().and_then(|name| name.to_str()) == Some(".cargo-lock")
            {
                lock_paths.push(path);
            }
        }
    }
    lock_paths.sort();
    let mut files: Vec<File> = Vec::with_capacity(lock_paths.len());
    for path in lock_paths {
        let file = match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(file) => file,
            Err(error) if lock_is_held(&error) => {
                for held in &files {
                    let _ = held.unlock();
                }
                return Ok(CargoLockProbe::Active(path));
            }
            Err(error) => return Err(error),
        };
        match file.try_lock_exclusive() {
            Ok(()) => files.push(file),
            Err(error) if lock_is_held(&error) => {
                for held in &files {
                    let _ = held.unlock();
                }
                return Ok(CargoLockProbe::Active(path));
            }
            Err(error) => return Err(error),
        }
    }
    Ok(CargoLockProbe::Idle(CargoLockGuard { files }))
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(persistent_unlocked_lock_is_idle, {
        let temp = tempfile::tempdir().unwrap();
        let lock = temp.path().join(".cargo-lock");
        File::create(&lock).unwrap();
        assert!(matches!(
            probe(temp.path()).unwrap(),
            CargoLockProbe::Idle(_)
        ));
    });

    crate::timed_test!(held_lock_is_active, {
        let temp = tempfile::tempdir().unwrap();
        let lock = temp.path().join(".cargo-lock");
        let holder = File::create(&lock).unwrap();
        holder.try_lock_exclusive().unwrap();
        assert!(matches!(
            probe(temp.path()).unwrap(),
            CargoLockProbe::Active(_)
        ));
        holder.unlock().unwrap();
    });
}
