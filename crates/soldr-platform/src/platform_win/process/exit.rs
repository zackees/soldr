//! Windows exit/signal interpretation.

/// How a process ended, classified from its exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminationKind {
    /// A normal exit with this code.
    Exit(i32),
    /// Terminated by a Unix signal (never produced on Windows).
    Signal,
    /// A negative Windows exit code, i.e. an NTSTATUS value.
    WindowsStatus(u32),
}

/// Classify an exit code. Windows reports abnormal terminations as negative
/// codes whose low bits carry the NTSTATUS value.
pub fn termination_kind(code: i32) -> TerminationKind {
    if code < 0 {
        TerminationKind::WindowsStatus(code as u32)
    } else {
        TerminationKind::Exit(code)
    }
}

/// Windows has no signals; abnormal termination shows up as an NTSTATUS.
pub fn is_signal_termination(_code: i32) -> bool {
    false
}

/// True when the OS refused to initialize the process at all.
///
/// `STATUS_DLL_INIT_FAILED` (0xC0000142) means the process died before
/// running any of its own code — a host process-creation failure, not a
/// build, cache, or toolchain error. (The watchdog's deliberate
/// `STATUS_STACK_BUFFER_OVERRUN` abort is *not* an init failure; it has its
/// own attribution path.)
pub fn is_init_failure(code: i32) -> bool {
    const STATUS_DLL_INIT_FAILED: u32 = 0xC000_0142;
    matches!(
        termination_kind(code),
        TerminationKind::WindowsStatus(STATUS_DLL_INIT_FAILED)
    )
}

/// Reconstruct a `std::process::ExitStatus` from a running-process-style
/// exit code (soldr#2546). Windows has no signal exits; negative values
/// pass through as the raw (NTSTATUS-shaped) process exit code.
pub fn exit_status_from_code(code: i32) -> std::process::ExitStatus {
    use std::os::windows::process::ExitStatusExt;
    std::process::ExitStatus::from_raw(code as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_init_failure_classification() {
        assert!(is_init_failure(0xC000_0142_u32 as i32));
        assert!(!is_init_failure(1));
        assert!(!is_init_failure(0xC000_0409_u32 as i32));
        assert!(!is_signal_termination(-1));
        assert_eq!(
            termination_kind(0xC000_0409_u32 as i32),
            TerminationKind::WindowsStatus(0xC000_0409)
        );
        assert_eq!(termination_kind(2), TerminationKind::Exit(2));
    }
}
