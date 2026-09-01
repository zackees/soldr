//! Linux PID inspection: liveness, zombie state, and image lookup.

use std::path::{Path, PathBuf};

/// Linux does not use the Windows console-attachment policy probe.
pub fn console_attached(_pid: u32) -> Option<bool> {
    None
}

/// A process observed running from inside a directory tree.
///
/// Linux cannot enumerate image paths process-wide without privileges
/// beyond `ps`, so [`holders_under`] answers empty here — the diagnosis it
/// feeds is Windows-specific (elsewhere an unlink succeeds against a
/// running image).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessHolder {
    /// The process id.
    pub pid: u32,
    /// The fully-resolved executable image path.
    pub exe: PathBuf,
}

/// True while `pid` names a live Linux process.
///
/// A zombie (an exited child awaiting reap) still answers `kill(pid, 0)`
/// but can never serve IPC again, so it is reported as dead.
pub fn is_alive(pid: u32) -> bool {
    // A pid the kernel cannot name is not alive. Callers hand us `u32` from
    // pid files, state rows and environment variables, and `pid_t` is signed:
    // 4294967295 would reach `kill` as -1, which means "every process I may
    // signal" and would answer this probe with a confident `true`.
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: kill(pid, 0) is a well-defined liveness probe — no signal is
    // delivered, the syscall just returns 0 if the pid exists and the
    // caller has permission to signal it.
    if unsafe { libc::kill(pid, 0) } != 0 {
        return false;
    }
    !is_zombie(pid as u32)
}

/// True when `pid` names a process that has exited but is still awaiting
/// collection by its parent.
pub fn is_zombie(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // The comm field is parenthesized and may itself contain spaces, so the
    // state character is the first byte after the LAST ") ".
    let Some((_, tail)) = stat.rsplit_once(") ") else {
        return false;
    };
    matches!(tail.as_bytes().first(), Some(b'Z' | b'X'))
}

/// Read the running process's executable path through procfs.
pub fn executable_path(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

/// True when the running image's file stem matches `expected_stem`.
/// Absence is deliberately a mismatch: callers use this check immediately
/// before signalling a PID, so an unavailable probe must never turn a stale
/// claim into authority to terminate an unrelated process.
pub fn executable_stem_matches(pid: u32, expected_stem: &str) -> bool {
    executable_path(pid)
        .as_deref()
        .and_then(Path::file_stem)
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem == expected_stem)
}

/// True when the running image resolves to `expected_path`.
pub fn executable_path_matches(pid: u32, expected_path: &Path) -> bool {
    let Some(actual) = executable_path(pid) else {
        return false;
    };
    let actual = std::fs::canonicalize(&actual).unwrap_or(actual);
    let expected = std::fs::canonicalize(expected_path)
        .unwrap_or_else(|_| expected_path.to_path_buf());
    actual == expected
}

/// Linux cannot walk process images without extra privileges; the
/// running-image deletion problem this diagnoses is Windows-specific.
pub fn holders_under(_dir: &Path) -> Vec<ProcessHolder> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_liveness_reports_current_process_alive() {
        assert!(is_alive(std::process::id()));
        assert!(!is_zombie(std::process::id()));
    }

    #[test]
    fn linux_image_path_resolves_for_current_process() {
        let path = executable_path(std::process::id());
        assert!(path.is_some(), "current process image must be readable");
    }
}
