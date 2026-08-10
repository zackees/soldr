use running_process::pty as core_pty;
use running_process::pty::NativePtyProcess as CoreNativePtyProcess;

// ── NativePtyProcess: empty argv errors ──

#[test]
fn pty_process_empty_argv_errors() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let result = CoreNativePtyProcess::new(vec![], None, None, 24, 80, None);
        assert!(result.is_err());
    });
}

// ── NativePtyProcess: start already started errors ──

#[test]
#[cfg(not(windows))]
fn pty_process_start_already_started_errors() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec![
            "python".to_string(),
            "-c".to_string(),
            "import time; time.sleep(0.1)".to_string(),
        ];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        process.start_impl().unwrap();
        let result = process.start_impl();
        assert!(result.is_err());
        let _ = process.close_impl();
    });
}

// ── Iteration 3: PTY Process Integration Tests ──

#[test]
fn pty_process_pid_none_before_start() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec!["python".to_string(), "-c".to_string(), "pass".to_string()];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        assert!(process.pid().unwrap().is_none());
    });
}

#[test]
#[cfg(not(windows))]
fn pty_process_lifecycle_start_wait_close() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec![
            "python".to_string(),
            "-c".to_string(),
            "print('hello')".to_string(),
        ];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        process.start_impl().unwrap();
        assert!(process.pid().unwrap().is_some());
        let code = process.wait_impl(Some(10.0)).unwrap();
        assert_eq!(code, 0);
        let _ = process.close_impl();
    });
}

#[test]
#[cfg(not(windows))]
fn pty_process_poll_none_while_running() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec![
            "python".to_string(),
            "-c".to_string(),
            "import time; time.sleep(5)".to_string(),
        ];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        process.start_impl().unwrap();
        assert!(
            core_pty::poll_pty_process(&process.handles, &process.returncode)
                .unwrap()
                .is_none()
        );
        let _ = process.close_impl();
    });
}

#[test]
#[cfg(not(windows))]
fn pty_process_nonzero_exit_code() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec![
            "python".to_string(),
            "-c".to_string(),
            "import sys; sys.exit(42)".to_string(),
        ];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        process.start_impl().unwrap();
        let code = process.wait_impl(Some(10.0)).unwrap();
        assert_eq!(code, 42);
        let _ = process.close_impl();
    });
}

#[test]
fn pty_process_write_before_start_errors() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec!["python".to_string(), "-c".to_string(), "pass".to_string()];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        assert!(process.write_impl(b"test", false).is_err());
    });
}

#[test]
#[cfg(not(windows))]
fn pty_process_input_metrics_tracked() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec![
            "python".to_string(),
            "-c".to_string(),
            "import time; time.sleep(2)".to_string(),
        ];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        process.start_impl().unwrap();
        assert_eq!(process.pty_input_bytes_total(), 0);
        let _ = process.write_impl(b"hello\n", false);
        assert_eq!(process.pty_input_bytes_total(), 6);
        assert_eq!(process.pty_newline_events_total(), 1);
        let _ = process.write_impl(b"x", true);
        assert_eq!(process.pty_submit_events_total(), 1);
        let _ = process.close_impl();
    });
}

#[test]
#[cfg(not(windows))]
fn pty_process_resize_while_running() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec![
            "python".to_string(),
            "-c".to_string(),
            "import time; time.sleep(2)".to_string(),
        ];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        process.start_impl().unwrap();
        assert!(process.resize_impl(40, 120).is_ok());
        let _ = process.close_impl();
    });
}

#[test]
#[cfg(not(windows))]
fn pty_process_kill_running_process() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec![
            "python".to_string(),
            "-c".to_string(),
            "import time; time.sleep(0.1)".to_string(),
        ];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        process.start_impl().unwrap();
        assert!(process.kill_impl().is_ok());
    });
}

#[test]
#[cfg(not(windows))]
fn pty_process_terminate_running_process() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec![
            "python".to_string(),
            "-c".to_string(),
            "import time; time.sleep(0.1)".to_string(),
        ];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        process.start_impl().unwrap();
        assert!(process.terminate_impl().is_ok());
        let _ = process.close_impl();
    });
}

#[test]
#[cfg(not(windows))]
fn pty_process_close_already_closed_is_noop() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec!["python".to_string(), "-c".to_string(), "pass".to_string()];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        process.start_impl().unwrap();
        let _ = process.wait_impl(Some(10.0));
        let _ = process.close_impl();
        assert!(process.close_impl().is_ok());
    });
}

#[test]
#[cfg(not(windows))]
fn pty_process_wait_timeout_errors() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec![
            "python".to_string(),
            "-c".to_string(),
            "import time; time.sleep(10)".to_string(),
        ];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        process.start_impl().unwrap();
        assert!(process.wait_impl(Some(0.1)).is_err());
        let _ = process.close_impl();
    });
}

#[test]
fn pty_process_send_interrupt_before_start_errors() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec!["python".to_string(), "-c".to_string(), "pass".to_string()];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        assert!(process.send_interrupt_impl().is_err());
    });
}

#[test]
fn pty_process_terminate_before_start_errors() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec!["python".to_string(), "-c".to_string(), "pass".to_string()];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        assert!(process.terminate_impl().is_err());
    });
}

#[test]
fn pty_process_kill_before_start_errors() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec!["python".to_string(), "-c".to_string(), "pass".to_string()];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        assert!(process.kill_impl().is_err());
    });
}

// ── NativePtyProcess mark_reader_closed / store_returncode tests ──

#[test]
fn pty_process_mark_reader_closed() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec!["python".to_string(), "-c".to_string(), "pass".to_string()];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        // reader should not be closed initially
        assert!(!process.reader.state.lock().unwrap().closed);
        process.mark_reader_closed();
        assert!(process.reader.state.lock().unwrap().closed);
    });
}

#[test]
fn pty_process_store_returncode_sets_value() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec!["python".to_string(), "-c".to_string(), "pass".to_string()];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        assert!(process.returncode.lock().unwrap().is_none());
        process.store_returncode(42);
        assert_eq!(*process.returncode.lock().unwrap(), Some(42));
    });
}

#[test]
fn pty_process_record_input_metrics_tracks_data() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec!["python".to_string(), "-c".to_string(), "pass".to_string()];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        assert_eq!(process.pty_input_bytes_total(), 0);
        process.record_input_metrics(b"hello\n", false);
        assert_eq!(process.pty_input_bytes_total(), 6);
        assert_eq!(process.pty_newline_events_total(), 1);
        assert_eq!(process.pty_submit_events_total(), 0);
        process.record_input_metrics(b"\r", true);
        assert_eq!(process.pty_submit_events_total(), 1);
    });
}

// ── Windows PTY process lifecycle tests ──
//
// Note: On Windows ConPTY, the child process cannot exit cleanly until
// the master pipe is dropped. Therefore `wait_impl()` alone may block
// indefinitely — use `close_impl()` (which drops handles then waits)
// for lifecycle cleanup. Tests that need the exit code must use
// `kill_impl()` which explicitly drops handles.

#[test]
#[cfg(windows)]
fn pty_process_start_and_close_windows() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec![
            "python".to_string(),
            "-c".to_string(),
            "print('hello')".to_string(),
        ];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        process.start_impl().unwrap();
        assert!(process.pid().unwrap().is_some());
        // close drops handles then waits — this is the correct Windows lifecycle
        assert!(process.close_impl().is_ok());
    });
}

#[test]
#[cfg(windows)]
fn pty_process_poll_none_while_running_windows() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec![
            "python".to_string(),
            "-c".to_string(),
            "import time; time.sleep(5)".to_string(),
        ];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        process.start_impl().unwrap();
        assert!(
            core_pty::poll_pty_process(&process.handles, &process.returncode)
                .unwrap()
                .is_none()
        );
        let _ = process.close_impl();
    });
}

#[test]
#[cfg(windows)]
fn pty_process_kill_running_process_windows() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec![
            "python".to_string(),
            "-c".to_string(),
            "import time; time.sleep(0.1)".to_string(),
        ];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        process.start_impl().unwrap();
        assert!(process.kill_impl().is_ok());
    });
}

#[test]
#[cfg(windows)]
fn pty_process_terminate_running_process_windows() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec![
            "python".to_string(),
            "-c".to_string(),
            "import time; time.sleep(0.1)".to_string(),
        ];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        process.start_impl().unwrap();
        // On Windows, terminate delegates to kill
        assert!(process.terminate_impl().is_ok());
    });
}

#[test]
#[cfg(windows)]
fn pty_process_close_not_started_is_ok_windows() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec!["python".to_string(), "-c".to_string(), "pass".to_string()];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        // close before start should be ok (handles are None)
        assert!(process.close_impl().is_ok());
    });
}

#[test]
#[cfg(windows)]
fn pty_process_start_already_started_errors_windows() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec![
            "python".to_string(),
            "-c".to_string(),
            "import time; time.sleep(0.1)".to_string(),
        ];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        process.start_impl().unwrap();
        let result = process.start_impl();
        assert!(result.is_err());
        let _ = process.close_impl();
    });
}

#[test]
#[cfg(windows)]
fn pty_process_resize_while_running_windows() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec![
            "python".to_string(),
            "-c".to_string(),
            "import time; time.sleep(2)".to_string(),
        ];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        process.start_impl().unwrap();
        assert!(process.resize_impl(40, 120).is_ok());
        let _ = process.close_impl();
    });
}

#[test]
#[cfg(windows)]
fn pty_process_write_windows() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec![
            "python".to_string(),
            "-c".to_string(),
            "import time; time.sleep(2)".to_string(),
        ];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        process.start_impl().unwrap();
        let _ = process.write_impl(b"hello\n", false);
        assert!(process.pty_input_bytes_total() >= 6);
        assert!(process.pty_newline_events_total() >= 1);
        let _ = process.close_impl();
    });
}

#[test]
#[cfg(windows)]
fn pty_process_input_metrics_tracked_windows() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec![
            "python".to_string(),
            "-c".to_string(),
            "import time; time.sleep(2)".to_string(),
        ];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        process.start_impl().unwrap();
        assert_eq!(process.pty_input_bytes_total(), 0);
        let _ = process.write_impl(b"hello\n", false);
        assert_eq!(process.pty_input_bytes_total(), 6);
        assert_eq!(process.pty_newline_events_total(), 1);
        let _ = process.write_impl(b"x", true);
        assert_eq!(process.pty_submit_events_total(), 1);
        let _ = process.close_impl();
    });
}

#[test]
#[cfg(windows)]
fn pty_process_send_interrupt_windows() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec![
            "python".to_string(),
            "-c".to_string(),
            "import time; time.sleep(0.1)".to_string(),
        ];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        process.start_impl().unwrap();
        // send_interrupt on Windows writes Ctrl+C byte via PTY
        assert!(process.send_interrupt_impl().is_ok());
        let _ = process.close_impl();
    });
}

#[test]
#[cfg(windows)]
fn pty_process_with_cwd_windows() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let tmp = std::env::temp_dir();
        let cwd = tmp.to_str().unwrap().to_string();
        let argv = vec!["python".to_string(), "-c".to_string(), "pass".to_string()];
        let process = CoreNativePtyProcess::new(argv, Some(cwd), None, 24, 80, None).unwrap();
        process.start_impl().unwrap();
        assert!(process.close_impl().is_ok());
    });
}

#[test]
#[cfg(windows)]
fn pty_process_with_env_windows() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let mut env_pairs = Vec::new();
        if let Ok(path) = std::env::var("PATH") {
            env_pairs.push(("PATH".to_string(), path));
        }
        if let Ok(root) = std::env::var("SystemRoot") {
            env_pairs.push(("SystemRoot".to_string(), root));
        }
        env_pairs.push(("RP_TEST_PTY".to_string(), "test_value".to_string()));
        let argv = vec![
            "python".to_string(),
            "-c".to_string(),
            "import os; print(os.environ.get('RP_TEST_PTY', 'MISSING'))".to_string(),
        ];
        let process = CoreNativePtyProcess::new(argv, None, Some(env_pairs), 24, 80, None).unwrap();
        process.start_impl().unwrap();
        assert!(process.close_impl().is_ok());
    });
}

// ── Windows PTY terminal input relay tests ──

#[test]
#[cfg(windows)]
fn pty_process_terminal_input_relay_not_active_initially() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        use std::sync::atomic::Ordering;
        let argv = vec!["python".to_string(), "-c".to_string(), "pass".to_string()];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        assert!(!process.terminal_input_relay_active.load(Ordering::Acquire));
    });
}

#[test]
#[cfg(windows)]
fn pty_process_stop_terminal_input_relay_noop_when_not_started() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec!["python".to_string(), "-c".to_string(), "pass".to_string()];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        process.stop_terminal_input_relay_impl(); // should not panic
    });
}

// ── Windows-specific helper function tests ──

#[test]
#[cfg(windows)]
fn assign_child_to_job_null_handle_errors() {
    use running_process::pty::assign_child_to_windows_kill_on_close_job;
    pyo3::Python::initialize();
    let result = assign_child_to_windows_kill_on_close_job(None);
    assert!(result.is_err());
}

#[test]
#[cfg(windows)]
fn apply_windows_pty_priority_none_handle_ok() {
    use running_process::pty::apply_windows_pty_priority;
    pyo3::Python::initialize();
    // None handle with any nice value should be Ok (early return)
    assert!(apply_windows_pty_priority(None, Some(5)).is_ok());
    assert!(apply_windows_pty_priority(None, None).is_ok());
}

#[test]
#[cfg(windows)]
fn apply_windows_pty_priority_zero_nice_noop() {
    use running_process::pty::apply_windows_pty_priority;
    pyo3::Python::initialize();
    // Some handle with nice=0 → flags=0 → early return Ok
    use std::os::windows::io::AsRawHandle;
    let current = std::process::Command::new("cmd")
        .args(["/C", "echo"])
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let handle = current.as_raw_handle();
    assert!(apply_windows_pty_priority(Some(handle), Some(0)).is_ok());
    assert!(apply_windows_pty_priority(Some(handle), None).is_ok());
}
