//! PID liveness, zombie state, and running-process image lookup.

pub use crate::platform_imp::process::inspect::{
    console_attached, executable_path, executable_path_matches, executable_stem_matches,
    holders_under, is_alive, is_zombie, process_start_token, ProcessHolder,
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
