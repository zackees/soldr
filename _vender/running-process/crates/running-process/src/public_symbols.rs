#![allow(improper_ctypes_definitions)]

#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use super::*;

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_process_start_public(
    process: &NativeProcess,
) -> Result<(), ProcessError> {
    process.start_impl()
}

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_process_wait_public(
    process: &NativeProcess,
    timeout: Option<Duration>,
) -> Result<i32, ProcessError> {
    process.wait_impl(timeout)
}

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_process_kill_public(
    process: &NativeProcess,
) -> Result<(), ProcessError> {
    process.kill_impl()
}

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_process_close_public(
    process: &NativeProcess,
) -> Result<(), ProcessError> {
    process.close_impl()
}

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_process_read_combined_public(
    process: &NativeProcess,
    timeout: Option<Duration>,
) -> ReadStatus<StreamEvent> {
    process.read_combined_impl(timeout)
}

/// Wait for capture finalization through the stable exported symbol.
///
/// A null pointer is accepted as a no-op. For a non-null pointer, the caller
/// must keep the borrowed [`NativeProcess`] alive for the duration of the call;
/// this function never takes ownership of or frees it.
///
/// # Safety
///
/// `process` must be null or point to a valid, initialized `NativeProcess`.
#[unsafe(no_mangle)]
#[inline(never)]
pub unsafe extern "C" fn rp_native_process_wait_for_capture_completion_public(
    process: *const NativeProcess,
) {
    let Some(process) = (unsafe { process.as_ref() }) else {
        return;
    };
    // No panic may cross a C ABI boundary. The void ABI is retained for
    // compatibility: both natural completion and forced deadline completion
    // return normally.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        process.finish_capture_drain_with_deadline(exported_capture_wait_deadline());
    }));
}

#[cfg(test)]
static EXPORTED_CAPTURE_WAIT_TIMEOUT_MS: AtomicU64 = AtomicU64::new(0);

fn exported_capture_wait_deadline() -> Instant {
    #[cfg(test)]
    {
        let timeout_ms = EXPORTED_CAPTURE_WAIT_TIMEOUT_MS.load(Ordering::Acquire);
        if timeout_ms != 0 {
            return Instant::now() + Duration::from_millis(timeout_ms);
        }
    }
    kill_drain_deadline()
}

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_process_wait_for_capture_completion_with_deadline_public(
    process: &NativeProcess,
    deadline: Instant,
) -> bool {
    process.wait_for_capture_completion_with_deadline_impl(deadline)
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use std::sync::{mpsc, Arc};

    static EXPORTED_WAIT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct TimeoutOverride;

    impl TimeoutOverride {
        fn set(timeout_ms: u64) -> Self {
            EXPORTED_CAPTURE_WAIT_TIMEOUT_MS.store(timeout_ms, Ordering::Release);
            Self
        }
    }

    impl Drop for TimeoutOverride {
        fn drop(&mut self) {
            EXPORTED_CAPTURE_WAIT_TIMEOUT_MS.store(0, Ordering::Release);
        }
    }

    fn capture_process() -> Arc<NativeProcess> {
        Arc::new(NativeProcess::new(ProcessConfig {
            command: CommandSpec::Argv(vec!["unused".into()]),
            cwd: None,
            env: None,
            capture: true,
            stderr_mode: StderrMode::Stdout,
            creationflags: None,
            create_process_group: false,
            stdin_mode: StdinMode::Inherit,
            nice: None,
        }))
    }

    fn close_fake_capture(process: &NativeProcess) {
        let mut queues = process.shared.queues.lock().expect("queue mutex poisoned");
        queues.stdout_closed = true;
        queues.stderr_closed = true;
        process.shared.condvar.notify_all();
    }

    #[test]
    fn exported_capture_wait_is_bounded_when_capture_never_closes() {
        let _test_guard = EXPORTED_WAIT_TEST_LOCK
            .lock()
            .expect("exported wait test mutex poisoned");
        let _timeout_override = TimeoutOverride::set(50);
        // Regression for #620: call the shipped symbol itself while its
        // production capture state remains open forever.
        let process = capture_process();
        let process_address = Arc::as_ptr(&process) as usize;
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let started = Instant::now();
            started_tx.send(()).expect("start receiver dropped");
            unsafe {
                rp_native_process_wait_for_capture_completion_public(
                    process_address as *const NativeProcess,
                );
            }
            done_tx
                .send(started.elapsed())
                .expect("completion receiver dropped");
        });
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("exported call never started");

        let timely = done_rx.recv_timeout(Duration::from_millis(500));
        close_fake_capture(&process);
        let elapsed = timely
            .or_else(|_| done_rx.recv_timeout(Duration::from_secs(1)))
            .expect("exported call did not unblock after capture closed");
        worker.join().expect("exported wait worker panicked");
        assert!(
            elapsed < Duration::from_millis(500),
            "exported capture wait remained unbounded for {elapsed:?}"
        );
    }

    #[test]
    fn exported_capture_wait_returns_for_normal_completion() {
        let _test_guard = EXPORTED_WAIT_TEST_LOCK
            .lock()
            .expect("exported wait test mutex poisoned");
        let process = capture_process();
        close_fake_capture(&process);
        let started = Instant::now();
        unsafe {
            rp_native_process_wait_for_capture_completion_public(Arc::as_ptr(&process));
        }
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn exported_capture_wait_accepts_null_without_taking_ownership() {
        let _test_guard = EXPORTED_WAIT_TEST_LOCK
            .lock()
            .expect("exported wait test mutex poisoned");
        unsafe {
            rp_native_process_wait_for_capture_completion_public(std::ptr::null());
        }
        let process = capture_process();
        let retained = Arc::clone(&process);
        close_fake_capture(&process);
        unsafe {
            rp_native_process_wait_for_capture_completion_public(Arc::as_ptr(&process));
        }
        assert_eq!(Arc::strong_count(&retained), 2);
    }

    #[test]
    fn exported_capture_wait_contains_internal_panics() {
        let _test_guard = EXPORTED_WAIT_TEST_LOCK
            .lock()
            .expect("exported wait test mutex poisoned");
        let process = capture_process();
        let poison_target = Arc::clone(&process);
        let _ = std::thread::spawn(move || {
            let _guard = poison_target
                .shared
                .queues
                .lock()
                .expect("queue mutex poisoned before test");
            panic!("injected queue poison");
        })
        .join();

        unsafe {
            rp_native_process_wait_for_capture_completion_public(Arc::as_ptr(&process));
        }
    }
}

#[cfg(windows)]
#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_assign_child_to_windows_kill_on_close_job_public(
    child: &Child,
) -> Result<WindowsJobHandle, std::io::Error> {
    assign_child_to_windows_kill_on_close_job_impl(child)
}

#[cfg(windows)]
#[inline(never)]
/// #539 slice 2 — observer-aware Job Object setup. When
/// `descendant_sink` is `Some`, the returned handle also owns an IOCP and
/// pump thread that emits descendant-lifecycle events. This is `extern
/// "Rust"` (not "C") because the `Sender<ObserverEvent>` parameter is not
/// ABI-stable; the older `_public` symbol is the C-ABI export.
pub fn rp_assign_child_to_windows_kill_on_close_job_with_observer_public(
    child: &Child,
    descendant_sink: Option<std::sync::mpsc::Sender<crate::observer::ObserverEvent>>,
    direct_pid: u32,
) -> Result<WindowsJobHandle, std::io::Error> {
    crate::windows::assign_child_to_windows_kill_on_close_job_with_observer_impl(
        child,
        descendant_sink,
        direct_pid,
    )
}
