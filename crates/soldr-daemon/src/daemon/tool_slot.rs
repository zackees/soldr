//! Client side of the compiler-adjacent tool slot (soldr#2877).
//!
//! The formatter is compiler-adjacent work that never reaches the compile
//! gate, because it is not a `CompileRequest`. Measured on a 4-core / 7.9 GiB
//! Linux container, `soldr cargo fmt --all` runs at most **two** formatters
//! totalling about **100 MiB** -- so bounding the formatter's own fan-out
//! would not have prevented the reported `ENOMEM`. What did matter is that
//! those ~100 MiB were invisible to the scheduler that was rationing
//! everything else: the failure is a *total*, and one contributor was not
//! being counted.
//!
//! So a formatter takes a slot in the same semaphore rather than getting a
//! semaphore of its own. The daemon does not run the tool; it holds the slot
//! while this process does.
//!
//! **Never fails the caller.** A formatter that refuses to run because the
//! daemon is busy, absent, or older than this client would be a worse
//! regression than the one this guards against. Every failure path degrades
//! to running unguarded, which is exactly today's behaviour.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::daemon::client::UnixOrPipe;
use crate::daemon::protocol::{Request, Response};

/// How long to keep asking for a slot before giving up and running anyway.
///
/// Long enough to outlast a burst of compiles, short enough that a wedged
/// daemon cannot stall a format indefinitely. On expiry the tool runs
/// unguarded rather than failing.
const ACQUIRE_BUDGET: Duration = Duration::from_secs(60);

/// Connect timeout per attempt. The daemon is local; this is a liveness
/// bound, not a queue wait -- queueing is expressed as `Backpressure`.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// An occupied compile slot, released when dropped.
///
/// Release happens by closing the connection, not by sending a message: a
/// tool killed for resource exhaustion -- the case this exists for -- would
/// never get to send one, and its slot would be stranded for the life of the
/// daemon.
pub struct ToolSlot {
    /// `None` when no slot was obtained. The tool still runs; it is simply
    /// not counted, which is the pre-soldr#2877 behaviour.
    _connection: Option<UnixOrPipe>,
    held: bool,
}

impl ToolSlot {
    /// A slot that was never obtained.
    pub fn unheld() -> Self {
        Self {
            _connection: None,
            held: false,
        }
    }

    /// Whether a slot is actually held, for diagnostics and tests.
    pub fn is_held(&self) -> bool {
        self.held
    }
}

/// Outcome of one acquisition attempt, so the retry loop reads as the state
/// machine it is rather than as nested matches.
#[derive(Debug, PartialEq, Eq)]
enum Attempt {
    /// Admitted.
    Granted,
    /// The queue is full; wait `retry_after` and ask again.
    Busy { retry_after: Duration },
    /// Nothing to wait for -- no daemon, an older protocol, a transport
    /// error. Run unguarded.
    Unavailable,
}

/// Classify one response to an `AcquireToolSlot`.
///
/// Split out and pure so the policy is testable without a daemon: the retry
/// behaviour is the whole point of this module and it is otherwise only
/// reachable through a live socket.
fn classify(response: &Response) -> Attempt {
    match response {
        Response::Ack => Attempt::Granted,
        Response::Backpressure { retry_after_ms } => Attempt::Busy {
            retry_after: Duration::from_millis(u64::from(*retry_after_ms).max(1)),
        },
        // A daemon that does not know this request answers `Error`, and an
        // older one fails the version check before answering at all. Both
        // mean "no slot exists here", not "wait".
        _ => Attempt::Unavailable,
    }
}

/// Whether this host can hold a slot open.
///
/// Windows keeps its pipe inside a tokio runtime on a worker thread (see
/// `compile_streaming_windows`), so holding one across an arbitrary child
/// process needs machinery this does not have yet. Rather than ship a
/// half-working path, Windows runs unguarded exactly as before -- and the
/// reported failure is a Linux container.
fn host_can_hold_a_slot() -> bool {
    crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Windows
}

/// Occupy one compile slot for `tool`, or return an unheld slot.
///
/// Blocks until admitted, until [`ACQUIRE_BUDGET`] expires, or until the
/// daemon says there is nothing to wait for.
pub fn acquire(sock_path: &Path, tool: &str) -> ToolSlot {
    if !host_can_hold_a_slot() {
        return ToolSlot::unheld();
    }
    let deadline = Instant::now() + ACQUIRE_BUDGET;
    loop {
        match try_acquire_once(sock_path, tool) {
            Ok((Attempt::Granted, connection)) => {
                return ToolSlot {
                    _connection: connection,
                    held: true,
                }
            }
            Ok((Attempt::Busy { retry_after }, _)) => {
                if Instant::now() + retry_after >= deadline {
                    return ToolSlot::unheld();
                }
                std::thread::sleep(retry_after);
            }
            Ok((Attempt::Unavailable, _)) | Err(()) => return ToolSlot::unheld(),
        }
    }
}

/// One connect-ask-read round trip.
///
/// The connection is returned with the outcome because a granted slot *is*
/// the open connection -- dropping it here would release the slot before the
/// caller ever had it.
fn try_acquire_once(sock_path: &Path, tool: &str) -> Result<(Attempt, Option<UnixOrPipe>), ()> {
    let mut stream =
        crate::daemon::client::connect_for_tool_slot(sock_path, CONNECT_TIMEOUT).map_err(|_| ())?;
    let request = Request::AcquireToolSlot {
        tool: tool.to_string(),
    };
    crate::daemon::ipc::write_frame_sync(&mut stream, &request).map_err(|_| ())?;
    let response: Response = crate::daemon::ipc::read_frame_sync(&mut stream).map_err(|_| ())?;
    let attempt = classify(&response);
    Ok((attempt, Some(stream)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ack_grants_the_slot() {
        assert_eq!(classify(&Response::Ack), Attempt::Granted);
    }

    #[test]
    fn backpressure_asks_the_caller_to_wait_for_the_named_delay() {
        assert_eq!(
            classify(&Response::Backpressure { retry_after_ms: 25 }),
            Attempt::Busy {
                retry_after: Duration::from_millis(25)
            }
        );
    }

    #[test]
    fn a_zero_retry_delay_still_sleeps() {
        // A daemon advertising 0 ms would turn the wait into a spin that
        // pins a core while the queue it is waiting on tries to drain.
        let Attempt::Busy { retry_after } = classify(&Response::Backpressure { retry_after_ms: 0 })
        else {
            panic!("zero delay must still be a wait");
        };
        assert!(retry_after >= Duration::from_millis(1));
    }

    #[test]
    fn an_error_means_run_unguarded_rather_than_wait() {
        // A daemon that predates this request answers Error. Retrying that
        // for the full budget would stall every format against an older
        // daemon for a minute and then run anyway.
        assert_eq!(
            classify(&Response::Error("unknown request".into())),
            Attempt::Unavailable
        );
    }

    #[test]
    fn an_unexpected_response_is_unavailable_not_granted() {
        // Defaulting an unrecognised reply to "granted" would report a slot
        // nobody is holding, which is worse than not counting the tool.
        assert_eq!(
            classify(&Response::TargetRegistryRows(Vec::new())),
            Attempt::Unavailable
        );
    }

    #[test]
    fn an_unheld_slot_reports_itself_as_unheld() {
        assert!(!ToolSlot::unheld().is_held());
    }

    #[test]
    fn a_windows_host_runs_unguarded() {
        // Asserted through the same predicate the caller uses, so the two
        // cannot drift: on Windows `acquire` must not even connect.
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            assert!(!host_can_hold_a_slot());
            assert!(!acquire(Path::new("does-not-exist"), "fmt").is_held());
        } else {
            assert!(host_can_hold_a_slot());
        }
    }

    #[test]
    fn an_absent_daemon_yields_an_unheld_slot_rather_than_blocking() {
        // The budget must never apply to "there is no daemon": that path has
        // nothing to wait for, and a minute of sleeping before every format
        // on a machine without a daemon would be its own bug.
        let started = Instant::now();
        let slot = acquire(Path::new("/nonexistent/soldr-tool-slot.sock"), "fmt");
        assert!(!slot.is_held());
        assert!(
            started.elapsed() < ACQUIRE_BUDGET,
            "waited {:?} for a socket that cannot exist",
            started.elapsed()
        );
    }
}
