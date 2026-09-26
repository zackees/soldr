//! PID liveness, zombie state, and running-process image lookup.

pub use crate::platform_imp::process::inspect::{
    child_pids, console_attached, executable_path, executable_path_matches,
    executable_stem_matches, holders_under, is_alive, is_zombie, process_start_token,
    working_directory, ProcessHolder,
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

    /// Where a host answers these probes at all, it must answer them about
    /// the live process asked for: its own working directory, and a child it
    /// just spawned — from a non-main thread, which is the case a reader of
    /// the main task's procfs file alone would miss.
    #[test]
    fn working_directory_and_children_describe_the_live_process() {
        use super::{child_pids, working_directory};
        if let Some(cwd) = working_directory(std::process::id()) {
            assert_eq!(
                std::fs::canonicalize(cwd).expect("canonical cwd"),
                std::fs::canonicalize(std::env::current_dir().expect("cwd")).expect("canonical")
            );
        }
        if child_pids(std::process::id()).is_none() {
            return;
        }
        // The spawning thread stays alive until the probe has run: a child
        // whose parent thread exits is re-parented to a surviving thread.
        let (pid_tx, pid_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let spawner = std::thread::spawn(move || {
            let mut child = std::process::Command::new("sleep")
                .arg("30")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn child from a worker thread");
            pid_tx.send(child.id()).expect("send pid");
            let _ = done_rx.recv();
            let _ = child.kill();
            let _ = child.wait();
        });
        let pid = pid_rx.recv().expect("child pid");
        let found = child_pids(std::process::id()).expect("children of a live process");
        done_tx.send(()).expect("release spawner");
        spawner.join().expect("spawner thread");
        assert!(found.contains(&pid), "{found:?} lacks {pid}");
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
