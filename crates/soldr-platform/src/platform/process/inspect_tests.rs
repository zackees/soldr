//! Facade tests for the working-directory, child-enumeration, and
//! file-holder probes.

use super::{child_pids, holders_of_file, working_directory, FileHolderScan};

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

/// Where a host can enumerate a file's holders at all (soldr#3581), it must
/// name this process while this process holds the file, and name nobody once
/// it lets go — the empty-answer contract the busy-lock diagnostic's "no
/// holder found" branch relies on.
///
/// Hosts that cannot enumerate answer `Unsupported` instead, which is the
/// other half of that contract: the diagnostic must say so rather than
/// report an empty list that would read as "nobody holds it".
#[test]
fn holders_of_file_names_this_process_only_while_it_holds_the_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let lock = dir.path().join("root-owner.lock");
    std::fs::write(&lock, b"").expect("touch lock");

    let during_handle = std::fs::File::open(&lock).expect("open lock holder");
    let FileHolderScan::Enumerated(during) = holders_of_file(&lock) else {
        return;
    };
    assert!(
        during.iter().any(|holder| holder.pid == std::process::id()),
        "this process holds the lock file and must be enumerated: {during:?}"
    );

    // Release the handle, then re-scan: the same file with no holder must
    // enumerate as empty, not as a stale entry for a process that let go.
    drop(during_handle);
    let FileHolderScan::Enumerated(after) = holders_of_file(&lock) else {
        return;
    };
    assert!(
        !after.iter().any(|holder| holder.pid == std::process::id()),
        "released the file, so this process must no longer be a holder: {after:?}"
    );
}
