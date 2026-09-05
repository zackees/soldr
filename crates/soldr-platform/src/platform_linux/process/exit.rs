//! Linux exit/signal interpretation.

/// How a process ended, classified from its exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminationKind {
    /// A normal exit with this code.
    Exit(i32),
    /// Terminated by a Unix signal.
    Signal,
    /// A negative Windows exit code, i.e. an NTSTATUS value (never
    /// produced on Linux).
    WindowsStatus(u32),
}

/// Classify an exit code. Current zccache encodes signal `N` as
/// `-(128 + N)`; `-1` remains the legacy unknown-signal sentinel.
pub fn termination_kind(code: i32) -> TerminationKind {
    if code == -1 || (-255..=-129).contains(&code) {
        TerminationKind::Signal
    } else {
        TerminationKind::Exit(code)
    }
}

/// True when the code represents signal termination.
pub fn is_signal_termination(code: i32) -> bool {
    termination_kind(code) == TerminationKind::Signal
}

/// True when the code equals the Windows `STATUS_DLL_INIT_FAILED`
/// (`0xC0000142`) NTSTATUS as sign-reinterpreted to `i32`.
///
/// This deliberately keeps the pre-migration host-independent comparison:
/// a real Unix `ExitStatus::code` is a 0-255 wait status and can never
/// produce this value, but callers classify exit codes that arrive through
/// other channels (wrapper reports) on every host, and the historical
/// contract is a pure constant comparison rather than a Windows-only probe.
pub fn is_init_failure(code: i32) -> bool {
    code == 0xC000_0142_u32 as i32
}

/// Reconstruct a `std::process::ExitStatus` from a running-process-style
/// exit code (soldr#2546): non-negative codes are normal exits, negative
/// values are `-signal` terminations.
pub fn exit_status_from_code(code: i32) -> std::process::ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    if code < 0 {
        std::process::ExitStatus::from_raw((-code) & 0x7f)
    } else {
        std::process::ExitStatus::from_raw((code & 0xff) << 8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_exit_classification() {
        assert!(is_signal_termination(-1));
        assert!(is_signal_termination(-143));
        assert!(is_signal_termination(-129));
        assert!(is_signal_termination(-255));
        assert!(!is_signal_termination(-128));
        assert!(!is_signal_termination(-256));
        assert!(!is_signal_termination(1));
        assert!(is_init_failure(0xC000_0142_u32 as i32));
        assert_eq!(termination_kind(-1), TerminationKind::Signal);
        assert_eq!(termination_kind(-143), TerminationKind::Signal);
        assert_eq!(termination_kind(2), TerminationKind::Exit(2));
    }
}
