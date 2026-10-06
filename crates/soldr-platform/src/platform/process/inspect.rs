//! PID liveness, zombie state, running-process image lookup, and the
//! enumeration of which live processes hold a given file open.

use std::path::PathBuf;

/// A live process observed holding a specific file open (soldr#3581).
///
/// Produced by [`holders_of_file`] on hosts that can enumerate a file's
/// openers. Every field is a best-effort reading: a holder can exit between
/// the enumeration and the identity read, and a host may refuse to expose
/// one field while still exposing the others, so `None` means "not readable
/// here" rather than "does not exist".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHolder {
    /// The holding process's id.
    pub pid: u32,
    /// The holder's running executable path, when the host lets the caller read it.
    pub exe: Option<PathBuf>,
    /// The holder's parent process id, when the host exposes one.
    pub parent_pid: Option<u32>,
    /// The holder's live children at enumeration time.
    ///
    /// Empty means "childless", which is half of the evidence behind a
    /// busy-lock diagnostic's orphan label; a `None`-shaped absence is
    /// deliberately not representable here, because claiming a holder is
    /// childless when its children could not be read would be a safety
    /// claim built on a failed probe.
    pub children: Vec<u32>,
}

/// The outcome of asking the host which live processes hold a file open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileHolderScan {
    /// The host enumerated holders. The list may be empty: nobody visible
    /// holds the file right now (the holder exited between checks, or lives
    /// where this scan is not allowed to look).
    Enumerated(Vec<FileHolder>),
    /// The host has no supported way to enumerate a file's holders. Callers
    /// must say so explicitly instead of implying that inspecting the file
    /// itself would name its holder.
    Unsupported,
}

pub use crate::platform_imp::process::inspect::{
    child_pids, console_attached, executable_path, executable_path_matches,
    executable_stem_matches, holders_of_file, holders_under, is_alive, is_zombie,
    process_start_token, working_directory, ProcessHolder,
};

#[cfg(test)]
mod tests {
    use super::process_start_token;

    /// The contract `process_start_token` promises callers (soldr-cli's
    /// broker route reaper, notably): a live process yields a stable, non-
    /// zero token across repeated reads, whichever OS-specific clock backs
    /// it. Pinned at the facade so it runs unconditionally on every host,
    /// not only the one whose per-OS implementation happens to be read.
    #[test]
    fn this_process_has_a_stable_non_zero_start_token() {
        let first = process_start_token(std::process::id());
        assert!(first.is_some(), "a live process must yield a start token");
        assert_ne!(first, Some(0), "a real process never boots at tick zero");
        assert_eq!(
            first,
            process_start_token(std::process::id()),
            "the token must not change between reads of the same live process"
        );
    }

    /// `None` means "cannot identify" and must never be produced for a pid
    /// that looks like it could match something real. `u32::MAX` and `0` are
    /// both impossible single-process ids on every supported platform.
    #[test]
    fn impossible_pids_yield_no_token() {
        assert_eq!(process_start_token(u32::MAX), None);
        assert_eq!(process_start_token(0), None);
    }
}

#[cfg(test)]
#[path = "inspect_tests.rs"]
mod probe_tests;
