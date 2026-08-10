//! Regression test for 4.0.1: the `PtyMaster` trait + `PtySize`
//! struct are reachable from downstream crates via the public
//! `running_process::pty::backend` re-exports, and `master.resize` /
//! `master.get_size` can be called through `NativePtyHandles.master`.
//!
//! In 4.0.0 the backend trait and module were `pub(crate)`, which
//! meant downstream consumers (e.g. clud's SIGWINCH relay) could
//! hold the `Box<dyn PtyMaster>` from `NativePtyHandles.master` but
//! couldn't call any method on it. This test locks the surface in.
//!
//! Uses `python -c sleep` as the child, matching the rest of the
//! integration test suite. If the test runner doesn't have a
//! `python` on PATH, the test is skipped via early return.

use std::time::{Duration, Instant};

use running_process::pty::backend::PtySize;
use running_process::pty::NativePtyProcess;

fn python_available() -> bool {
    std::process::Command::new("python")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn pty_master_resize_and_get_size_through_handles() {
    if !python_available() {
        eprintln!("[skip] python not on PATH");
        return;
    }

    let process = NativePtyProcess::new(
        vec![
            "python".into(),
            "-c".into(),
            "import time; time.sleep(5)".into(),
        ],
        None,
        None,
        24,
        80,
        None,
    )
    .expect("construct pty");
    process.start_impl().expect("start pty");

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if process
            .handles
            .lock()
            .expect("pty handles mutex poisoned")
            .is_some()
        {
            break;
        }
        if Instant::now() >= deadline {
            panic!("handles never populated after start");
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    {
        let guard = process.handles.lock().expect("pty handles mutex poisoned");
        let handles = guard.as_ref().expect("handles populated");

        // Initial size should match openpty.
        let initial = handles.master.get_size().expect("get_size after openpty");
        assert_eq!(
            initial,
            PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0
            }
        );

        // Resize → get_size returns the new value.
        let new_size = PtySize {
            rows: 40,
            cols: 132,
            pixel_width: 0,
            pixel_height: 0,
        };
        handles.master.resize(new_size).expect("resize");
        let observed = handles.master.get_size().expect("get_size after resize");
        assert_eq!(observed, new_size);
    }

    let _ = process.kill_impl();
}
