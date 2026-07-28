#[cfg(all(test, unix))]
mod pid_liveness_tests {
    use crate::daemon::lifecycle::*;
    use std::time::{Duration, Instant};

    // An exited-but-unreaped child must read as stopped.
    //
    // `kill(pid, 0)` succeeds for a zombie on every unix, so without a
    // per-platform state probe `wait_for_shutdown_responder` never observes
    // `Exited` and burns its whole timeout. This regression is silent on Linux
    // (which has `/proc/<pid>/stat`) and fatal on macOS, which is exactly how
    // it reached CI.
    crate::timed_test!(exited_unreaped_child_is_not_alive, {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn probe child");
        let pid = child.id();

        // Deliberately do NOT reap before probing — a reaped pid disappears
        // from the process table and would pass for the wrong reason.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut observed_stopped = false;
        while Instant::now() < deadline {
            if !pid_is_alive(pid) {
                observed_stopped = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }

        // Reap only after the assertion input is captured.
        let _ = child.wait();
        assert!(
            observed_stopped,
            "an exited, unreaped child must not report as alive; \
             pid {pid} still looked live after 10s"
        );
    });
}
