//! Diagnostics for compiler processes terminated outside their own control.

const SIGNAL_DIAGNOSTIC: &[u8] = b"soldr: compiler process was terminated by a Unix signal; under concurrent build load this can indicate an OOM/resource-limit kill. Reduce CARGO_BUILD_JOBS and SOLDR_JOBS, or inspect the host's memory-pressure counters (soldr#2453).\n";

/// zccache currently represents `ExitStatus::code() == None` as `-1`.
///
/// Without a diagnostic, the wrapper's exit guard misclassifies that result as
/// an internal SESSION transport fault. Preserve the compiler's stderr and add
/// the actionable distinction at the soldr/zccache response boundary.
pub(crate) fn annotate_signal_termination(exit_code: i32, stderr: Vec<u8>) -> Vec<u8> {
    let mut stderr = stderr;
    if crate::platform::process::exit::is_signal_termination(exit_code) {
        if !stderr.is_empty() && !stderr.ends_with(b"\n") {
            stderr.push(b'\n');
        }
        stderr.extend_from_slice(SIGNAL_DIAGNOSTIC);
    }
    stderr
}

#[cfg(test)]
mod tests {
    use super::*;

    // Host-neutral: the per-host classification of `-1` lives in the
    // platform crate's exit tests; here the assertion is conditional on the
    // platform's own answer so the diagnostic path is covered on Unix hosts
    // and the no-op path is covered on Windows hosts.
    #[test]
    fn signal_termination_gets_an_actionable_diagnostic() {
        let stderr = annotate_signal_termination(-1, Vec::new());
        if crate::platform::process::exit::is_signal_termination(-1) {
            assert_eq!(stderr, SIGNAL_DIAGNOSTIC);
        }
    }

    #[test]
    fn compiler_stderr_is_preserved_before_the_diagnostic() {
        let original = b"rustc said why".to_vec();
        let stderr = annotate_signal_termination(-1, original.clone());
        if crate::platform::process::exit::is_signal_termination(-1) {
            assert!(stderr.starts_with(b"rustc said why\n"));
            assert!(stderr.ends_with(SIGNAL_DIAGNOSTIC));
        } else {
            assert_eq!(stderr, original);
        }
    }

    #[test]
    fn ordinary_exit_codes_are_byte_identical() {
        let original = b"ordinary compiler error\n".to_vec();
        assert_eq!(annotate_signal_termination(1, original.clone()), original);
    }
}
