//! Windows `DuplicateHandle` handoff transport model.
//!
//! This module owns the broker-side `DuplicateHandle` call used to pass an
//! already-accepted client pipe into a backend process. The backend still has
//! to verify the one-time token before adopting the connection; failures map
//! into the existing silent reconnect fallback policy.

use super::{
    HandoffAttemptDecision, HandoffAttemptFailure, HandoffFallbackDecision, HandoffFallbackReason,
    HandoffToken,
};

/// Whether this build target can eventually use the Windows handoff transport.
pub const DUPLICATE_HANDLE_TRANSPORT_SUPPORTED: bool = cfg!(windows);

/// Opaque raw Windows handle value held by the broker or duplicated into a backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WindowsHandleValue(usize);

impl WindowsHandleValue {
    /// Build an opaque handle value for transport bookkeeping.
    pub fn new(value: usize) -> Self {
        Self(value)
    }

    /// Return the raw opaque handle value.
    pub fn get(self) -> usize {
        self.0
    }

    #[cfg(windows)]
    fn from_handle(handle: windows_sys::Win32::Foundation::HANDLE) -> Self {
        Self(handle as usize)
    }

    #[cfg(windows)]
    fn as_handle(self) -> windows_sys::Win32::Foundation::HANDLE {
        self.0 as windows_sys::Win32::Foundation::HANDLE
    }
}

/// Inputs for one future `DuplicateHandle` attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DuplicateHandleAttempt {
    /// Broker-owned pipe handle to duplicate.
    pub pipe_handle: WindowsHandleValue,
    /// Backend process ID that should receive the duplicated handle.
    pub backend_pid: u32,
    /// One-time token associated with this handoff attempt.
    pub handoff_token: HandoffToken,
}

impl DuplicateHandleAttempt {
    /// Build typed inputs for one `DuplicateHandle` attempt.
    pub fn new(
        pipe_handle: WindowsHandleValue,
        backend_pid: u32,
        handoff_token: HandoffToken,
    ) -> Self {
        Self {
            pipe_handle,
            backend_pid,
            handoff_token,
        }
    }
}

/// Successful `DuplicateHandle` outcome once real handle passing is wired.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DuplicateHandleSuccess {
    /// Handle value duplicated into the backend process.
    pub duplicated_handle: WindowsHandleValue,
    /// Backend process ID that received the duplicated handle.
    pub backend_pid: u32,
    /// One-time token paired with the duplicated handle.
    pub handoff_token: HandoffToken,
}

impl DuplicateHandleSuccess {
    /// Build a typed successful handoff result.
    pub fn new(
        duplicated_handle: WindowsHandleValue,
        backend_pid: u32,
        handoff_token: HandoffToken,
    ) -> Self {
        Self {
            duplicated_handle,
            backend_pid,
            handoff_token,
        }
    }
}

/// Result returned by the future Windows transport.
pub type DuplicateHandleResult = Result<DuplicateHandleSuccess, DuplicateHandleError>;

/// Try to duplicate the broker-held pipe handle into the backend process.
///
/// The returned handle value is valid in the backend process handle table.
/// Callers must still deliver the paired [`HandoffToken`] over the
/// broker-to-backend control channel and wait for backend acknowledgement
/// before reporting handoff success to the client.
pub fn try_duplicate_handle(attempt: &DuplicateHandleAttempt) -> DuplicateHandleResult {
    platform_try_duplicate_handle(attempt)
}

/// Failure from a future `DuplicateHandle` handoff attempt.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DuplicateHandleError {
    /// The current target cannot use the Windows handoff transport.
    #[error("DuplicateHandle handoff transport is unsupported on this platform")]
    UnsupportedPlatform,
    /// Opening the backend process for `PROCESS_DUP_HANDLE` failed.
    #[error("cannot open backend process {backend_pid} for DuplicateHandle")]
    CannotOpenBackend {
        /// Backend process ID that could not be opened.
        backend_pid: u32,
    },
    /// The platform denied handle duplication.
    #[error("permission denied duplicating handle into backend process {backend_pid}")]
    PermissionDenied {
        /// Backend process ID targeted by the handoff.
        backend_pid: u32,
    },
    /// `DuplicateHandle` failed after the backend process was opened.
    #[error("DuplicateHandle failed for backend process {backend_pid}")]
    DuplicateFailed {
        /// Backend process ID targeted by the handoff.
        backend_pid: u32,
        /// Raw Windows error code returned by the platform, when available.
        raw_os_error: Option<i32>,
    },
    /// The broker and backend trust or integrity levels are incompatible.
    #[error("integrity mismatch duplicating handle into backend process {backend_pid}")]
    IntegrityMismatch {
        /// Backend process ID targeted by the handoff.
        backend_pid: u32,
    },
    /// The backend did not acknowledge the duplicated handle before the deadline.
    #[error("backend process {backend_pid} did not acknowledge duplicated handle")]
    BackendAckTimeout {
        /// Backend process ID targeted by the handoff.
        backend_pid: u32,
    },
}

#[cfg(windows)]
fn platform_try_duplicate_handle(attempt: &DuplicateHandleAttempt) -> DuplicateHandleResult {
    use windows_sys::Win32::Foundation::{
        CloseHandle, DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, PROCESS_DUP_HANDLE,
    };

    let backend_process = unsafe { OpenProcess(PROCESS_DUP_HANDLE, 0, attempt.backend_pid) };
    if is_invalid_handle(backend_process) {
        return Err(open_process_error(attempt.backend_pid));
    }

    let mut duplicated: HANDLE = std::ptr::null_mut();
    let ok = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            attempt.pipe_handle.as_handle(),
            backend_process,
            &mut duplicated,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    };
    let duplicate_error = std::io::Error::last_os_error();
    unsafe {
        CloseHandle(backend_process);
    }

    if ok == 0 || is_invalid_handle(duplicated) {
        return Err(duplicate_handle_error(
            attempt.backend_pid,
            duplicate_error.raw_os_error(),
        ));
    }

    Ok(DuplicateHandleSuccess::new(
        WindowsHandleValue::from_handle(duplicated),
        attempt.backend_pid,
        attempt.handoff_token,
    ))
}

#[cfg(not(windows))]
fn platform_try_duplicate_handle(_attempt: &DuplicateHandleAttempt) -> DuplicateHandleResult {
    Err(DuplicateHandleError::UnsupportedPlatform)
}

#[cfg(windows)]
fn is_invalid_handle(handle: windows_sys::Win32::Foundation::HANDLE) -> bool {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;

    handle.is_null() || handle == INVALID_HANDLE_VALUE
}

#[cfg(windows)]
fn open_process_error(backend_pid: u32) -> DuplicateHandleError {
    use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;

    if std::io::Error::last_os_error().raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) {
        DuplicateHandleError::PermissionDenied { backend_pid }
    } else {
        DuplicateHandleError::CannotOpenBackend { backend_pid }
    }
}

#[cfg(windows)]
fn duplicate_handle_error(backend_pid: u32, raw_os_error: Option<i32>) -> DuplicateHandleError {
    use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;

    if raw_os_error == Some(ERROR_ACCESS_DENIED as i32) {
        DuplicateHandleError::PermissionDenied { backend_pid }
    } else {
        DuplicateHandleError::DuplicateFailed {
            backend_pid,
            raw_os_error,
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn duplicate_handle_into_current_process_returns_backend_owned_handle() {
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Threading::GetCurrentProcess;

        let token = HandoffToken::from_bytes([7; 16]);
        let attempt = DuplicateHandleAttempt::new(
            WindowsHandleValue::new(unsafe { GetCurrentProcess() } as usize),
            std::process::id(),
            token,
        );

        let success = try_duplicate_handle(&attempt).unwrap();

        assert_eq!(success.backend_pid, std::process::id());
        assert_eq!(success.handoff_token, token);
        assert_ne!(success.duplicated_handle.get(), 0);
        assert_ne!(
            success.duplicated_handle.get(),
            INVALID_HANDLE_VALUE as usize
        );

        unsafe {
            CloseHandle(success.duplicated_handle.get() as HANDLE);
        }
    }

    #[test]
    fn missing_backend_pid_maps_to_fallback_safe_error() {
        let attempt = DuplicateHandleAttempt::new(
            WindowsHandleValue::new(unsafe {
                windows_sys::Win32::System::Threading::GetCurrentProcess()
            } as usize),
            u32::MAX,
            HandoffToken::from_bytes([9; 16]),
        );

        let err = try_duplicate_handle(&attempt).unwrap_err();

        assert!(matches!(
            err,
            DuplicateHandleError::CannotOpenBackend { .. }
                | DuplicateHandleError::PermissionDenied { .. }
        ));
        assert!(err.is_fallback_safe());
    }
}

impl DuplicateHandleError {
    /// Return the existing attempt-failure classification, when this was a real attempt.
    pub fn attempt_failure(&self) -> Option<HandoffAttemptFailure> {
        match self {
            Self::UnsupportedPlatform => None,
            Self::CannotOpenBackend { .. }
            | Self::PermissionDenied { .. }
            | Self::DuplicateFailed { .. } => Some(HandoffAttemptFailure::PermissionDenied),
            Self::IntegrityMismatch { .. } => Some(HandoffAttemptFailure::IntegrityMismatch),
            Self::BackendAckTimeout { .. } => Some(HandoffAttemptFailure::BackendAckTimeout),
        }
    }

    /// Map this transport failure into the existing fallback reason vocabulary.
    pub fn fallback_reason(&self) -> HandoffFallbackReason {
        match self.attempt_failure() {
            Some(failure) => failure.into(),
            None => HandoffFallbackReason::ServicePolicyDisabled,
        }
    }

    /// Return the silent reconnect fallback for this transport failure.
    pub fn fallback_decision(&self) -> HandoffFallbackDecision {
        HandoffFallbackDecision::new(self.fallback_reason())
    }

    /// Return the full attempt decision for callers that operate on broker decisions.
    pub fn fallback_attempt_decision(&self) -> HandoffAttemptDecision {
        HandoffAttemptDecision::FallbackToReconnect(self.fallback_decision())
    }

    /// Return true when this error is safe to hide behind reconnect fallback.
    pub fn is_fallback_safe(&self) -> bool {
        let fallback = self.fallback_decision();
        fallback.uses_backend_reconnect() && !fallback.sends_client_error()
    }
}
