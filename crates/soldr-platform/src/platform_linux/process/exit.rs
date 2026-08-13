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

/// Classify an exit code. The zccache wrapper contract represents
/// `ExitStatus::code() == None` (signal termination) as `-1`.
pub fn termination_kind(code: i32) -> TerminationKind {
    if code == -1 {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test] // allow-bare-test: soldr-platform is a dependency leaf; timed_test! lives in soldr-core (#2493)
    fn linux_exit_classification() {
        assert!(is_signal_termination(-1));
        assert!(!is_signal_termination(1));
        assert!(is_init_failure(0xC000_0142_u32 as i32));
        assert_eq!(termination_kind(-1), TerminationKind::Signal);
        assert_eq!(termination_kind(2), TerminationKind::Exit(2));
    }
}
