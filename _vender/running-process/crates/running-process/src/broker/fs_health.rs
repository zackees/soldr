//! Filesystem-health probes for status/doctor visibility (#390).
//!
//! Inode usage matters on Unix filesystems with fixed inode tables
//! (ext4 most prominently): the daemon data dir can fail writes with
//! ENOSPC while plenty of bytes remain free. Windows filesystems have no
//! fixed inode table, so the probe reports "not applicable" there instead
//! of faking numbers.

use std::path::Path;

/// Inode totals for one filesystem, from `statvfs` on Unix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InodeUsage {
    /// Total inodes on the filesystem (`f_files`).
    pub total: u64,
    /// Inodes available to unprivileged users (`f_favail`).
    pub free: u64,
}

impl InodeUsage {
    /// Inodes currently in use.
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.free)
    }

    /// Used fraction in `[0.0, 1.0]`; `0.0` when the total is zero.
    pub fn used_ratio(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.used() as f64 / self.total as f64
        }
    }
}

/// Probe inode usage for the filesystem containing `path`.
///
/// Returns `Ok(None)` when inode accounting does not apply: always on
/// Windows, and on Unix filesystems that report a zero inode table
/// (e.g. btrfs). Errors are real probe failures (missing path, EACCES).
pub fn inode_usage(path: &Path) -> std::io::Result<Option<InodeUsage>> {
    #[cfg(windows)]
    {
        let _ = path;
        Ok(None)
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;

        let bytes = path.as_os_str().as_bytes();
        let c_path = std::ffi::CString::new(bytes)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidInput, err))?;
        let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stats) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if stats.f_files == 0 {
            return Ok(None);
        }
        // fsfilcnt_t is u64 on Linux but u32 on macOS; keep explicit casts.
        #[allow(clippy::unnecessary_cast)]
        let usage = InodeUsage {
            total: stats.f_files as u64,
            free: stats.f_favail as u64,
        };
        Ok(Some(usage))
    }
}

/// Probe inode usage for the daemon data directory (where the SQLite
/// tracking database lives), walking up to the nearest existing ancestor
/// so the probe stays read-only even before the daemon ever ran.
pub fn daemon_data_dir_inode_usage() -> std::io::Result<Option<InodeUsage>> {
    let dir = crate::client::paths::data_dir();
    let mut probe: &Path = &dir;
    while !probe.exists() {
        match probe.parent() {
            Some(parent) => probe = parent,
            None => break,
        }
    }
    inode_usage(probe)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn used_ratio_handles_zero_total() {
        let usage = InodeUsage { total: 0, free: 0 };
        assert_eq!(usage.used_ratio(), 0.0);
    }

    #[test]
    fn used_ratio_is_fractional() {
        let usage = InodeUsage {
            total: 100,
            free: 25,
        };
        assert_eq!(usage.used(), 75);
        assert!((usage.used_ratio() - 0.75).abs() < f64::EPSILON);
    }

    #[cfg(unix)]
    #[test]
    fn inode_usage_probes_temp_dir() {
        let result = inode_usage(&std::env::temp_dir()).expect("statvfs on temp dir");
        if let Some(usage) = result {
            assert!(usage.total > 0);
            assert!(usage.free <= usage.total);
        }
    }

    #[cfg(windows)]
    #[test]
    fn inode_usage_is_not_applicable_on_windows() {
        let result = inode_usage(&std::env::temp_dir()).expect("probe never fails on windows");
        assert_eq!(result, None);
    }

    #[test]
    fn daemon_data_dir_probe_never_panics() {
        let _ = daemon_data_dir_inode_usage();
    }
}
