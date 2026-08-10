//! Shared helpers used by multiple handler sub-modules.

use crate::proto::daemon::{DaemonResponse, StatusCode};

/// Build an error `DaemonResponse` with no payload.
pub(super) fn error_response(request_id: u64, code: StatusCode, message: String) -> DaemonResponse {
    DaemonResponse {
        request_id,
        code: code as i32,
        message,
        ..Default::default()
    }
}

/// Variant used by the PTY/pipe session handlers. Behaviourally identical
/// to [`error_response`], but kept as a separate symbol because the
/// original file uses both names — preserving them avoids touching any
/// call sites.
pub(super) fn error_pty_response(
    request_id: u64,
    code: StatusCode,
    message: String,
) -> DaemonResponse {
    DaemonResponse {
        request_id,
        code: code as i32,
        message,
        ..Default::default()
    }
}

/// Current wall-clock time as fractional seconds since the UNIX epoch.
pub(super) fn unix_now_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// Convert a PTY/pipe session [`crate::daemon::pty_sessions::TerminationOutcome`]
/// to its proto representation.
pub(super) fn termination_outcome_to_proto(
    outcome: crate::daemon::pty_sessions::TerminationOutcome,
) -> crate::proto::daemon::TerminationOutcome {
    use crate::daemon::pty_sessions::TerminationOutcome as T;
    use crate::proto::daemon::TerminationOutcome as ProtoTerminationOutcome;
    match outcome {
        T::Unspecified => ProtoTerminationOutcome::Unspecified,
        T::NaturalExit => ProtoTerminationOutcome::NaturalExit,
        T::SoftExit => ProtoTerminationOutcome::SoftExit,
        T::HardKilled => ProtoTerminationOutcome::HardKilled,
    }
}
