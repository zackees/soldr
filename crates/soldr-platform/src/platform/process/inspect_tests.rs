//! Facade tests for the working-directory and child-enumeration probes.

use super::{child_pids, working_directory};

/// Where a host answers these probes at all, it must answer them about
/// the live process asked for: its own working directory, and a child it
/// just spawned — from a non-main thread, which is the case a reader of
/// the main task's procfs file alone would miss.
#[test]
fn working_directory_and_children_describe_the_live_process() {
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
