use std::fs::OpenOptions;
use std::io::Write;
#[cfg(any(test, unix))]
use std::sync::Mutex;
#[cfg(any(test, unix))]
use std::thread;
use std::time::{Duration, Instant};

pub(crate) const CHILD_PID_LOG_PATH_ENV: &str = "RUNNING_PROCESS_CHILD_PID_LOG_PATH";

/// Hard cap on how long `kill_impl()` will block on
/// `wait_for_capture_completion` after the direct child has been
/// reaped. Override via the `RUNNING_PROCESS_KILL_DRAIN_TIMEOUT_MS`
/// env var (milliseconds). The default of 2 s gives normal children
/// time to flush their pipe buffers while preventing indefinite hangs
/// when a grandchild inherits the pipe and outlives the parent (FastLED
/// Bug B).
pub(crate) const DEFAULT_KILL_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
pub(crate) const KILL_DRAIN_TIMEOUT_ENV: &str = "RUNNING_PROCESS_KILL_DRAIN_TIMEOUT_MS";

pub(crate) fn kill_drain_deadline() -> Instant {
    let timeout = std::env::var(KILL_DRAIN_TIMEOUT_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_KILL_DRAIN_TIMEOUT);
    Instant::now() + timeout
}

#[cfg(any(test, unix))]
pub(crate) fn poll_until<T>(
    deadline: Instant,
    interval: Duration,
    mut poll: impl FnMut() -> std::io::Result<Option<T>>,
) -> std::io::Result<Option<T>> {
    loop {
        if let Some(value) = poll()? {
            return Ok(Some(value));
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(None);
        }
        thread::sleep(interval.min(deadline.saturating_duration_since(now)));
    }
}

#[cfg(any(test, unix))]
pub(crate) fn poll_mutex_until<S, T>(
    state: &Mutex<S>,
    deadline: Instant,
    interval: Duration,
    mut poll: impl FnMut(&mut S) -> std::io::Result<Option<T>>,
) -> std::io::Result<Option<T>> {
    poll_until(deadline, interval, || {
        let mut guard = state.lock().expect("child mutex poisoned");
        poll(&mut guard)
    })
}

#[cfg(any(test, unix))]
pub(crate) fn completed_reap_after_signal<T>(result: std::io::Result<Option<T>>) -> Option<T> {
    // Once termination was successfully delivered, reaping is best effort on
    // the caller's bounded path. The dedicated waiter remains responsible for
    // eventual reaping after either a timeout or a transient try_wait error.
    result.ok().flatten()
}

#[cfg(any(test, unix))]
pub(crate) fn child_try_wait_error_is_retryable(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::Interrupted
}

#[cfg(any(test, unix))]
pub(crate) fn with_child_lock_for_signal<S, T>(
    state: &Mutex<S>,
    signal: impl FnOnce(&mut S) -> T,
) -> T {
    let mut guard = state.lock().expect("child mutex poisoned");
    signal(&mut guard)
}

#[cfg(any(test, unix))]
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ChildSignalDisposition<T> {
    AlreadyExited(T),
    Signal,
}

#[cfg(any(test, unix))]
pub(crate) fn child_signal_disposition<T>(
    status: std::io::Result<Option<T>>,
) -> std::io::Result<ChildSignalDisposition<T>> {
    status.map(|status| match status {
        Some(status) => ChildSignalDisposition::AlreadyExited(status),
        None => ChildSignalDisposition::Signal,
    })
}

pub(crate) fn log_spawned_child_pid(pid: u32) -> Result<(), std::io::Error> {
    let Some(path) = std::env::var_os(CHILD_PID_LOG_PATH_ENV) else {
        return Ok(());
    };

    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(format!("{pid}\n").as_bytes())?;
    file.flush()?;
    Ok(())
}

pub(crate) fn feed_chunk(pending: &mut Vec<u8>, chunk: &[u8]) -> Vec<Vec<u8>> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut index = 0;

    while index < chunk.len() {
        if chunk[index] == b'\n' {
            let end = if index > start && chunk[index - 1] == b'\r' {
                index - 1
            } else {
                index
            };
            pending.extend_from_slice(&chunk[start..end]);
            if !pending.is_empty() {
                lines.push(std::mem::take(pending));
            }
            start = index + 1;
        }
        index += 1;
    }

    pending.extend_from_slice(&chunk[start..]);
    lines
}

pub(crate) fn exit_code(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status
            .code()
            .unwrap_or_else(|| -status.signal().unwrap_or(1))
    }
    #[cfg(not(unix))]
    {
        status.code().unwrap_or(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc};

    #[test]
    fn mutex_poll_releases_lock_between_attempts() {
        let state = Arc::new(Mutex::new(()));
        let worker_state = Arc::clone(&state);
        let (polled_tx, polled_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            poll_mutex_until(
                &worker_state,
                // Long enough that the observation window below fits entirely
                // inside it — see the note there.
                Instant::now() + Duration::from_secs(4),
                Duration::from_millis(50),
                |_| {
                    let _ = polled_tx.send(());
                    Ok::<_, std::io::Error>(None::<()>)
                },
            )
        });

        polled_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("poll never started");
        // How long to wait for the lock to become free. This has to stay
        // comfortably *inside* the worker's polling deadline above: once the
        // worker finishes it drops the lock for good, so a window that
        // outlived it would report success without ever observing a release
        // between polls — the test would pass no matter how the lock is held.
        //
        // 40ms was too tight to survive a loaded machine (seen failing in a
        // parallel full-suite sweep), so both bounds were raised together
        // rather than just this one.
        let available_deadline = Instant::now() + Duration::from_secs(1);
        let mut available = false;
        while Instant::now() < available_deadline {
            if state.try_lock().is_ok() {
                available = true;
                break;
            }
            thread::yield_now();
        }
        assert!(
            available,
            "child mutex remained locked between bounded polls"
        );
        assert_eq!(worker.join().expect("poll worker panicked").unwrap(), None);
    }

    #[test]
    fn mutex_poll_returns_ready_value_exactly_once() {
        let attempts = AtomicUsize::new(0);
        let value = poll_mutex_until(
            &Mutex::new(()),
            Instant::now() + Duration::from_secs(1),
            Duration::from_millis(10),
            |_| {
                attempts.fetch_add(1, Ordering::SeqCst);
                Ok::<_, std::io::Error>(Some(17))
            },
        )
        .unwrap();

        assert_eq!(value, Some(17));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn successful_signal_treats_reap_error_like_timeout() {
        let error = std::io::Error::other("injected try_wait failure");
        assert_eq!(completed_reap_after_signal::<i32>(Err(error)), None);
    }

    #[test]
    fn exit_waiter_retries_only_interrupted_try_wait_errors() {
        let interrupted = std::io::Error::from(std::io::ErrorKind::Interrupted);
        let terminal = std::io::Error::other("injected terminal error");
        assert!(child_try_wait_error_is_retryable(&interrupted));
        assert!(!child_try_wait_error_is_retryable(&terminal));
    }

    #[test]
    fn child_lock_covers_pid_validation_through_signal_delivery() {
        let state = Arc::new(Mutex::new(7_u32));
        let worker_state = Arc::clone(&state);
        let (entered_tx, entered_rx) = mpsc::channel();
        let release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let worker_release = Arc::clone(&release);
        let worker = thread::spawn(move || {
            with_child_lock_for_signal(&worker_state, |pid| {
                assert_eq!(*pid, 7);
                entered_tx.send(()).expect("signal checkpoint receiver");
                let (lock, condvar) = &*worker_release;
                let mut released = lock.lock().expect("release mutex poisoned");
                while !*released {
                    released = condvar.wait(released).expect("release mutex poisoned");
                }
            });
        });
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("signal closure never started");
        assert!(
            state.try_lock().is_err(),
            "child lock was released between PID validation and signaling"
        );
        *release.0.lock().expect("release mutex poisoned") = true;
        release.1.notify_all();
        worker.join().expect("signal worker panicked");
        assert!(state.try_lock().is_ok());
    }

    #[test]
    fn already_reaped_child_never_reaches_signal_delivery() {
        let status = child_signal_disposition(Ok(Some(23))).unwrap();
        assert_eq!(status, ChildSignalDisposition::AlreadyExited(23));
    }
}
