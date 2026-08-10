//! FD-pressure self-demotion for the broker accept path (#390).
//!
//! When `accept()` fails with EMFILE/ENFILE the broker is out of file
//! descriptors. Instead of failing accepts opaquely, the broker demotes
//! itself: connections that do get through receive a structured
//! `Refused` reply carrying the reserved `ERROR_FD_PRESSURE` code (slot 9
//! in the frozen v1 envelope), and admin verbs keep working so operators
//! can see the demoted state in `status --json`. The guard recovers
//! automatically once a configurable streak of accepts succeeds again.
//!
//! The guard is a small pure state machine behind interior mutability so
//! the accept loop, the admin snapshot provider, and tests can all share
//! one instance without real fd exhaustion.

use std::io;
use std::sync::Mutex;

use crate::broker::protocol::{ErrorCode, HelloReply};

use super::connection::refused_reply;

/// Consecutive successful accepts required to clear a demotion.
pub const DEFAULT_FD_PRESSURE_RECOVERY_ACCEPTS: u32 = 3;
/// `Refused.retry_after_ms` hint sent while demoted.
pub const DEFAULT_FD_PRESSURE_RETRY_AFTER_MS: u64 = 1_000;

/// Outcome of feeding one accept error into the guard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FdPressureDecision {
    /// The error was fd exhaustion; the broker is now (still) demoted.
    Demoted,
    /// The error was unrelated to fd pressure; caller handles it normally.
    Unrelated,
}

/// Tunables for [`FdPressureGuard`].
#[derive(Clone, Copy, Debug)]
pub struct FdPressureConfig {
    /// Consecutive successful accepts required to clear a demotion.
    pub recovery_accepts: u32,
    /// `Refused.retry_after_ms` hint sent while demoted.
    pub retry_after_ms: u64,
}

impl Default for FdPressureConfig {
    fn default() -> Self {
        Self {
            recovery_accepts: DEFAULT_FD_PRESSURE_RECOVERY_ACCEPTS,
            retry_after_ms: DEFAULT_FD_PRESSURE_RETRY_AFTER_MS,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct GuardState {
    demoted: bool,
    consecutive_ok: u32,
    demotions_total: u64,
    refused_while_demoted: u64,
}

/// Shared fd-pressure demotion state machine.
#[derive(Debug, Default)]
pub struct FdPressureGuard {
    config: FdPressureConfig,
    state: Mutex<GuardState>,
}

impl FdPressureGuard {
    /// Build a guard with explicit tunables.
    pub fn new(config: FdPressureConfig) -> Self {
        Self {
            config,
            state: Mutex::new(GuardState::default()),
        }
    }

    /// Classify one accept error. Fd-exhaustion errors demote the broker;
    /// anything else is reported back to the caller as unrelated.
    pub fn on_accept_error(&self, err: &io::Error) -> FdPressureDecision {
        if !is_fd_exhaustion_error(err) {
            return FdPressureDecision::Unrelated;
        }
        let mut state = self.lock();
        if !state.demoted {
            state.demoted = true;
            state.demotions_total += 1;
        }
        state.consecutive_ok = 0;
        FdPressureDecision::Demoted
    }

    /// Record one successful accept. Returns `true` when this accept
    /// cleared an active demotion.
    pub fn on_accept_ok(&self) -> bool {
        let mut state = self.lock();
        if !state.demoted {
            return false;
        }
        state.consecutive_ok += 1;
        if state.consecutive_ok >= self.config.recovery_accepts {
            state.demoted = false;
            state.consecutive_ok = 0;
            return true;
        }
        false
    }

    /// Whether new Hello connections are currently being refused.
    pub fn is_demoted(&self) -> bool {
        self.lock().demoted
    }

    /// Total demotion episodes since the guard was created.
    pub fn demotions_total(&self) -> u64 {
        self.lock().demotions_total
    }

    /// Hello connections refused with `ERROR_FD_PRESSURE` so far.
    pub fn refused_while_demoted(&self) -> u64 {
        self.lock().refused_while_demoted
    }

    /// Structured refusal sent to Hello clients while demoted.
    pub fn refusal_reply(&self) -> HelloReply {
        self.lock().refused_while_demoted += 1;
        refused_reply(
            ErrorCode::ErrorFdPressure,
            "broker is low on file descriptors; retry shortly",
            self.config.retry_after_ms,
        )
    }

    /// Force a demotion without an accept error (tests / external probes).
    pub fn force_demote(&self) {
        let mut state = self.lock();
        if !state.demoted {
            state.demoted = true;
            state.demotions_total += 1;
        }
        state.consecutive_ok = 0;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, GuardState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// True when `err` signals process- or system-wide fd exhaustion.
pub fn is_fd_exhaustion_error(err: &io::Error) -> bool {
    let Some(code) = err.raw_os_error() else {
        return false;
    };
    #[cfg(unix)]
    {
        code == libc::EMFILE || code == libc::ENFILE
    }
    #[cfg(windows)]
    {
        // WSAEMFILE, ERROR_TOO_MANY_OPEN_FILES, ERROR_NO_SYSTEM_RESOURCES.
        code == 10024 || code == 4 || code == 1450
    }
}

/// One platform-appropriate fd-exhaustion raw error code (test helper).
pub fn fd_exhaustion_error_for_tests() -> io::Error {
    #[cfg(unix)]
    {
        io::Error::from_raw_os_error(libc::EMFILE)
    }
    #[cfg(windows)]
    {
        io::Error::from_raw_os_error(10024)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broker::protocol::hello_reply::Result as HelloReplyResult;

    #[test]
    fn unrelated_errors_do_not_demote() {
        let guard = FdPressureGuard::default();
        let err = io::Error::new(io::ErrorKind::PermissionDenied, "denied");
        assert_eq!(guard.on_accept_error(&err), FdPressureDecision::Unrelated);
        assert!(!guard.is_demoted());
        assert_eq!(guard.demotions_total(), 0);
    }

    #[test]
    fn fd_exhaustion_demotes_and_recovers_after_streak() {
        let guard = FdPressureGuard::new(FdPressureConfig {
            recovery_accepts: 2,
            retry_after_ms: 250,
        });
        assert_eq!(
            guard.on_accept_error(&fd_exhaustion_error_for_tests()),
            FdPressureDecision::Demoted
        );
        assert!(guard.is_demoted());
        assert_eq!(guard.demotions_total(), 1);

        assert!(!guard.on_accept_ok());
        assert!(guard.is_demoted());
        assert!(guard.on_accept_ok());
        assert!(!guard.is_demoted());
    }

    #[test]
    fn accept_error_resets_recovery_streak() {
        let guard = FdPressureGuard::new(FdPressureConfig {
            recovery_accepts: 2,
            retry_after_ms: 250,
        });
        guard.on_accept_error(&fd_exhaustion_error_for_tests());
        assert!(!guard.on_accept_ok());
        guard.on_accept_error(&fd_exhaustion_error_for_tests());
        assert!(!guard.on_accept_ok());
        assert!(guard.is_demoted());
        assert!(guard.on_accept_ok());
        assert!(!guard.is_demoted());
        assert_eq!(guard.demotions_total(), 1);
    }

    #[test]
    fn refusal_reply_uses_reserved_fd_pressure_code() {
        let guard = FdPressureGuard::default();
        guard.force_demote();
        let reply = guard.refusal_reply();
        let HelloReplyResult::Refused(refused) = reply.result.unwrap() else {
            panic!("expected refusal");
        };
        assert_eq!(
            ErrorCode::try_from(refused.code),
            Ok(ErrorCode::ErrorFdPressure)
        );
        assert_eq!(refused.retry_after_ms, DEFAULT_FD_PRESSURE_RETRY_AFTER_MS);
        assert_eq!(guard.refused_while_demoted(), 1);
    }

    #[test]
    fn ok_accepts_while_healthy_are_no_ops() {
        let guard = FdPressureGuard::default();
        assert!(!guard.on_accept_ok());
        assert!(!guard.is_demoted());
    }
}
