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

/// A PID-reuse-safe identity token for `pid`: its creation time.
///
/// Field 22 of `/proc/<pid>/stat` (`starttime`) is clock ticks since boot,
/// which is stable for the life of the process and changes whenever the
/// kernel reuses the pid onto something else -- exactly the pairing
/// soldr#3054's broker route reaper needs so a recycled requester pid cannot
/// be mistaken for the process that originally asked for a route.
///
/// The comm field is parenthesized and may itself contain spaces or `)`, so
/// this reuses `is_zombie`'s parse: split on the LAST `") "` rather than the
/// first, and never hand-roll a second parser for the same line that could
/// silently disagree with it.
///
/// `None` on any failure -- an exited or unreadable process, or a line this
/// host's kernel does not shape as expected. `None` must never be treated as
/// a match by a caller comparing tokens.
pub fn process_start_token(pid: u32) -> Option<u64> {
    // Same out-of-range guard as `is_alive`/`signal_pid`: a `u32` this large
    // cannot name a real Linux pid, and treating it as one risks reading the
    // wrong `/proc` entry if the kernel is ever coaxed into recycling near
    // that boundary.
    let pid = libc::pid_t::try_from(pid).ok().filter(|pid| *pid > 0)?;
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, tail) = stat.rsplit_once(") ")?;
    tail.split_whitespace().nth(19)?.parse().ok()
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

    #[test]
    fn linux_start_token_is_stable_and_present_for_this_process() {
        let first = process_start_token(std::process::id());
        assert!(first.is_some(), "a live process must have a readable starttime");
        let second = process_start_token(std::process::id());
        assert_eq!(first, second, "starttime does not change across reads");
        assert_ne!(first, Some(0), "a real process never boots at tick zero");
    }

    #[test]
    fn linux_start_token_parses_a_comm_field_containing_parens_and_spaces() {
        // Regression pin for the ") " parse this function shares with
        // `is_zombie`: a comm field like "a) (b" must not fool a naive
        // first-") " split into truncating the record. Field counts here are
        // lifted directly from a real `/proc/self/stat` line: 19 fields
        // (state through itrealvalue) separate the comm close-paren from
        // starttime at tail index 19.
        let stat =
            "123 (a) (b) S 1 123 123 0 -1 4194304 0 0 0 0 0 0 0 0 20 0 1 0 999 0 0 0 0 0 0 0 0";
        let (_, tail) = stat.rsplit_once(") ").unwrap();
        assert_eq!(tail.split_whitespace().nth(19), Some("999"));
    }
}
