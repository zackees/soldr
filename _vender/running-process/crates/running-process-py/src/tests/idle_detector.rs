use crate::idle_detector::NativeIdleDetector;
use crate::signal_bool::NativeSignalBool;

// ── NativeIdleDetector tests (requires PyO3) ──

pub(crate) fn make_idle_detector(
    py: pyo3::Python<'_>,
    timeout_seconds: f64,
    enabled: bool,
    initial_idle_for: f64,
) -> NativeIdleDetector {
    let signal = pyo3::Py::new(py, NativeSignalBool::new(enabled)).unwrap();
    NativeIdleDetector::new(
        py,
        timeout_seconds,
        0.0,  // stability_window_seconds
        0.01, // sample_interval_seconds
        signal,
        true, // reset_on_input
        true, // reset_on_output
        true, // count_control_churn_as_output
        initial_idle_for,
    )
}

#[test]
fn idle_detector_mark_exit_returns_process_exit() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let det = make_idle_detector(py, 10.0, true, 0.0);
        det.mark_exit(42, false);
        let (triggered, reason, _idle_for, returncode) = det.wait(py, Some(1.0));
        assert!(!triggered);
        assert_eq!(reason, "process_exit");
        assert_eq!(returncode, Some(42));
    });
}

#[test]
fn idle_detector_mark_exit_interrupted() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let det = make_idle_detector(py, 10.0, true, 0.0);
        det.mark_exit(1, true);
        let (triggered, reason, _idle_for, returncode) = det.wait(py, Some(1.0));
        assert!(!triggered);
        assert_eq!(reason, "interrupt");
        assert_eq!(returncode, Some(1));
    });
}

#[test]
fn idle_detector_timeout_when_not_idle() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let det = make_idle_detector(py, 10.0, true, 0.0);
        let (triggered, reason, _idle_for, returncode) = det.wait(py, Some(0.05));
        assert!(!triggered);
        assert_eq!(reason, "timeout");
        assert!(returncode.is_none());
    });
}

#[test]
fn idle_detector_triggers_when_already_idle() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        // initial_idle_for=1.0 means it thinks it's been idle for 1 second
        // timeout_seconds=0.5 means 0.5s idle triggers
        let det = make_idle_detector(py, 0.5, true, 1.0);
        let (triggered, reason, _idle_for, _returncode) = det.wait(py, Some(1.0));
        assert!(triggered);
        assert_eq!(reason, "idle_timeout");
    });
}

#[test]
fn idle_detector_disabled_does_not_trigger() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let det = make_idle_detector(py, 0.01, false, 1.0);
        let (triggered, reason, _idle_for, _returncode) = det.wait(py, Some(0.1));
        assert!(!triggered);
        assert_eq!(reason, "timeout");
    });
}

#[test]
fn idle_detector_record_input_resets_idle() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let det = make_idle_detector(py, 0.5, true, 1.0);
        // Recording input should reset the idle timer
        det.record_input(5);
        // Now it should NOT trigger immediately since we just reset
        let (triggered, reason, _idle_for, _returncode) = det.wait(py, Some(0.05));
        assert!(!triggered);
        assert_eq!(reason, "timeout");
    });
}

#[test]
fn idle_detector_record_output_resets_idle() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let det = make_idle_detector(py, 0.5, true, 1.0);
        // Recording visible output should reset idle timer
        det.record_output(b"visible output");
        let (triggered, reason, _idle_for, _returncode) = det.wait(py, Some(0.05));
        assert!(!triggered);
        assert_eq!(reason, "timeout");
    });
}

#[test]
fn idle_detector_control_churn_only_no_reset_when_not_counted() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let signal = pyo3::Py::new(py, NativeSignalBool::new(true)).unwrap();
        let det = NativeIdleDetector::new(
            py, 0.05, 0.0, 0.01, signal, true, true,
            false, // count_control_churn_as_output = false
            1.0,   // already idle for 1s
        );
        // Output only ANSI escape (no visible content)
        det.record_output(b"\x1b[31m");
        // Should still trigger because control churn doesn't count
        let (triggered, reason, _idle_for, _returncode) = det.wait(py, Some(0.5));
        assert!(triggered);
        assert_eq!(reason, "idle_timeout");
    });
}

// ── NativeIdleDetector additional tests ──

#[test]
fn idle_detector_record_input_zero_bytes_no_reset() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let det = make_idle_detector(py, 0.05, true, 1.0);
        // Recording 0 bytes should NOT reset idle timer
        det.record_input(0);
        let (triggered, reason, _idle_for, _returncode) = det.wait(py, Some(0.5));
        assert!(triggered);
        assert_eq!(reason, "idle_timeout");
    });
}

#[test]
fn idle_detector_record_output_empty_no_reset() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let det = make_idle_detector(py, 0.05, true, 1.0);
        // Recording empty output should NOT reset idle timer
        det.record_output(b"");
        let (triggered, reason, _idle_for, _returncode) = det.wait(py, Some(0.5));
        assert!(triggered);
        assert_eq!(reason, "idle_timeout");
    });
}

#[test]
fn idle_detector_enabled_getter_and_setter() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let det = make_idle_detector(py, 1.0, true, 0.0);
        assert!(det.enabled());
        det.set_enabled(false);
        assert!(!det.enabled());
        det.set_enabled(true);
        assert!(det.enabled());
    });
}

// ── NativeIdleDetector: additional wait/record scenarios ──

#[test]
fn idle_detector_wait_idle_timeout_with_initial_idle() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let signal = pyo3::Py::new(py, NativeSignalBool::new(true)).unwrap();
        let detector =
            NativeIdleDetector::new(py, 0.01, 0.01, 0.001, signal, true, true, true, 100.0);
        let (idle, reason, _, code) = detector.wait(py, Some(1.0));
        assert!(idle);
        assert_eq!(reason, "idle_timeout");
        assert!(code.is_none());
    });
}

#[test]
fn idle_detector_record_output_only_control_churn_with_flag() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let signal = pyo3::Py::new(py, NativeSignalBool::new(true)).unwrap();
        let detector = NativeIdleDetector::new(py, 1.0, 0.5, 0.1, signal, true, true, true, 5.0);
        let state_before = detector.core.state.lock().unwrap().last_reset_at;
        std::thread::sleep(std::time::Duration::from_millis(10));
        detector.record_output(b"\x1b[H");
        let state_after = detector.core.state.lock().unwrap().last_reset_at;
        assert!(state_after > state_before);
    });
}

#[test]
fn idle_detector_record_output_only_control_churn_without_flag() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let signal = pyo3::Py::new(py, NativeSignalBool::new(true)).unwrap();
        let detector = NativeIdleDetector::new(py, 1.0, 0.5, 0.1, signal, true, true, false, 5.0);
        let state_before = detector.core.state.lock().unwrap().last_reset_at;
        std::thread::sleep(std::time::Duration::from_millis(10));
        detector.record_output(b"\x1b[H");
        let state_after = detector.core.state.lock().unwrap().last_reset_at;
        assert_eq!(state_before, state_after);
    });
}

#[test]
fn idle_detector_record_output_not_enabled() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let signal = pyo3::Py::new(py, NativeSignalBool::new(true)).unwrap();
        let detector = NativeIdleDetector::new(py, 1.0, 0.5, 0.1, signal, true, false, true, 5.0);
        let state_before = detector.core.state.lock().unwrap().last_reset_at;
        std::thread::sleep(std::time::Duration::from_millis(10));
        detector.record_output(b"visible");
        let state_after = detector.core.state.lock().unwrap().last_reset_at;
        assert_eq!(state_before, state_after);
    });
}

#[test]
fn idle_detector_record_input_not_enabled() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let signal = pyo3::Py::new(py, NativeSignalBool::new(true)).unwrap();
        let detector = NativeIdleDetector::new(py, 1.0, 0.5, 0.1, signal, false, true, true, 5.0);
        let state_before = detector.core.state.lock().unwrap().last_reset_at;
        std::thread::sleep(std::time::Duration::from_millis(10));
        detector.record_input(100);
        let state_after = detector.core.state.lock().unwrap().last_reset_at;
        assert_eq!(state_before, state_after);
    });
}

#[test]
fn idle_detector_record_input_nonzero_bytes_resets() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let signal = pyo3::Py::new(py, NativeSignalBool::new(true)).unwrap();
        let detector = NativeIdleDetector::new(py, 1.0, 0.5, 0.1, signal, true, true, true, 5.0);
        let state_before = detector.core.state.lock().unwrap().last_reset_at;
        std::thread::sleep(std::time::Duration::from_millis(10));
        detector.record_input(100);
        let state_after = detector.core.state.lock().unwrap().last_reset_at;
        assert!(state_after > state_before);
    });
}

#[test]
fn idle_detector_record_output_visible_resets() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let signal = pyo3::Py::new(py, NativeSignalBool::new(true)).unwrap();
        let detector = NativeIdleDetector::new(py, 1.0, 0.5, 0.1, signal, true, true, true, 5.0);
        let state_before = detector.core.state.lock().unwrap().last_reset_at;
        std::thread::sleep(std::time::Duration::from_millis(10));
        detector.record_output(b"visible output");
        let state_after = detector.core.state.lock().unwrap().last_reset_at;
        assert!(state_after > state_before);
    });
}

#[test]
fn idle_detector_mark_exit_sets_returncode() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let signal = pyo3::Py::new(py, NativeSignalBool::new(true)).unwrap();
        let detector = NativeIdleDetector::new(py, 1.0, 0.5, 0.1, signal, true, true, true, 0.0);
        detector.mark_exit(42, false);
        let state = detector.core.state.lock().unwrap();
        assert_eq!(state.returncode, Some(42));
        assert!(!state.interrupted);
    });
}
