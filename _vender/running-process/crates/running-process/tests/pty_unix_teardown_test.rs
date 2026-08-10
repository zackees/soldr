#![cfg(unix)]

use std::sync::mpsc;
use std::time::{Duration, Instant};

use running_process::pty::NativePtyProcess;

// The implementation has separate 2s child-reap and 2s reader-teardown
// budgets. Allow the documented 4s worst case plus scheduling headroom.
const OPERATION_DEADLINE: Duration = Duration::from_secs(5);
const CLEANUP_DEADLINE: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug)]
enum Teardown {
    Close,
    Kill,
}

fn python_available() -> bool {
    std::process::Command::new("python")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn start_python(script: &str) -> NativePtyProcess {
    let process = NativePtyProcess::new(
        vec!["python".into(), "-c".into(), script.into()],
        None,
        None,
        24,
        80,
        None,
    )
    .expect("construct PTY process");
    process.start_impl().expect("start PTY process");
    process
}

fn wait_for_ready(process: &NativePtyProcess) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut output = Vec::new();
    while Instant::now() < deadline {
        match process.read_chunk_impl(Some(0.1)) {
            Ok(Some(chunk)) => {
                output.extend_from_slice(&chunk);
                if output.windows(b"READY".len()).any(|part| part == b"READY") {
                    return;
                }
            }
            Ok(None) => {}
            Err(err) => panic!(
                "PTY reader closed before READY: {err}; output={:?}",
                String::from_utf8_lossy(&output)
            ),
        }
    }
    panic!(
        "timed out waiting for READY; output={:?}",
        String::from_utf8_lossy(&output)
    );
}

fn assert_teardown_bounded(process: NativePtyProcess, teardown: Teardown) {
    let process_group = process
        .pid()
        .expect("query PTY process group")
        .expect("PTY process group missing");
    let (tx, rx) = mpsc::channel();
    let started = Instant::now();
    std::thread::spawn(move || {
        let result = match teardown {
            Teardown::Close => process.close_impl(),
            Teardown::Kill => process.kill_impl(),
        };
        let _ = tx.send(result);
    });

    match rx.recv_timeout(OPERATION_DEADLINE) {
        Ok(result) => result.unwrap_or_else(|err| panic!("{teardown:?} failed: {err}")),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            panic!("{teardown:?} worker disconnected before returning")
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // Cleanup the exact PTY process group so the RED case cannot leak
            // the stubborn child or slave-retaining grandchild into later tests.
            // SAFETY: a negative PID targets only the captured process group.
            unsafe {
                libc::kill(-(process_group as i32), libc::SIGKILL);
            }
            let _ = rx.recv_timeout(CLEANUP_DEADLINE);
            panic!(
                "{teardown:?} exceeded {OPERATION_DEADLINE:?} (elapsed {:?}); \
                 Unix PTY teardown is not bounded",
                started.elapsed()
            );
        }
    }
}

fn ignores_sighup(teardown: Teardown) {
    if !python_available() {
        eprintln!("[skip] python not on PATH");
        return;
    }
    let process = start_python(
        "import signal,time; \
         signal.signal(signal.SIGHUP, signal.SIG_IGN); \
         print('READY', flush=True); \
         time.sleep(60)",
    );
    wait_for_ready(&process);
    assert_teardown_bounded(process, teardown);
}

fn grandchild_retains_slave_pty(teardown: Teardown) {
    if !python_available() {
        eprintln!("[skip] python not on PATH");
        return;
    }
    let process = start_python(
        "import os,signal,time; \
         pid=os.fork(); \
         (signal.signal(signal.SIGHUP, signal.SIG_IGN), time.sleep(60)) \
             if pid == 0 else (print('READY', pid, flush=True), os._exit(0))",
    );
    wait_for_ready(&process);
    assert_teardown_bounded(process, teardown);
}

#[test]
fn close_is_bounded_when_child_ignores_sighup() {
    // Regression for #606 trigger (a).
    ignores_sighup(Teardown::Close);
}

#[test]
fn kill_is_bounded_when_child_ignores_sighup() {
    // Regression for #606 trigger (a).
    ignores_sighup(Teardown::Kill);
}

#[test]
fn close_is_bounded_when_grandchild_retains_slave_pty() {
    // Regression for #606 trigger (b).
    grandchild_retains_slave_pty(Teardown::Close);
}

#[test]
fn kill_is_bounded_when_grandchild_retains_slave_pty() {
    // Regression for #606 trigger (b).
    grandchild_retains_slave_pty(Teardown::Kill);
}
