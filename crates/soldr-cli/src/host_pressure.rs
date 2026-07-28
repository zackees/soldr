//! Explain OS-level process-creation failures that are not build errors.
//!
//! soldr#1974. When the host cannot supply the resources to start a
//! process, Windows refuses to initialize its DLLs and the child dies
//! before `main` with `STATUS_DLL_INIT_FAILED`. Nothing was wrong with the
//! code, the toolchain, or the cache -- but every layer above reports it as
//! though something was:
//!
//! - cargo prints `process didn't exit successfully: ... (exit code:
//!   0xc0000142, STATUS_DLL_INIT_FAILED)`, which reads as a crash
//! - when the victim is `link.exe`, rustc appends *"the Visual Studio build
//!   tools may need to be repaired"* -- actively misleading advice that
//!   sends people to reinstall a toolchain that is fine
//!
//! The condition is transient and host-wide: it strikes whichever process
//! happens to start next, so the victim rotates between runs and an
//! identical retry usually succeeds. That combination -- rotating victim,
//! passes on retry, plausible-looking toolchain error -- is what makes it
//! expensive to diagnose.
//!
//! This module recognizes the one exit code that is unambiguously this
//! condition and says so.

use std::io::Write;

/// `STATUS_DLL_INIT_FAILED` (`0xC0000142`) as it appears through
/// [`std::process::ExitStatus::code`].
///
/// Windows surfaces the NTSTATUS as the process exit code, and Rust widens
/// it to `i32`, so the comparison is against the sign-reinterpreted value
/// (`-1073741502`) rather than the literal.
///
/// Not gated to `cfg(windows)`: a Unix `ExitStatus::code` is a 0-255 wait
/// status and can never collide with this value, so the check is harmless
/// there and the tests run on every platform.
pub(crate) const STATUS_DLL_INIT_FAILED: i32 = 0xC000_0142_u32 as i32;

/// True when `exit_code` is the process-initialization failure above.
pub(crate) fn is_process_init_failure(exit_code: i32) -> bool {
    exit_code == STATUS_DLL_INIT_FAILED
}

/// The advisory printed when a compiler dies at process init.
///
/// Kept separate from the writer so tests can assert the wording without
/// capturing stderr.
///
/// # Why this names no single cause
///
/// The first version of this message asserted handle / paged-pool
/// exhaustion. That was wrong, and shipped: `0xC0000142` was then observed
/// on a host with **205,910 total handles and a 8,356-handle top holder** --
/// entirely healthy. Several distinct resources produce this same code:
///
/// - **desktop heap** (`SharedSection` in the `SubSystems` registry value) --
///   a fixed shared section consumed per-process and *unrelated* to the
///   handle table. A build spawning many short-lived compilers exhausts it
///   while handles stay flat, which is exactly the observed case.
/// - **handle / paged-pool exhaustion** -- typically one leaking process
///   holding hundreds of thousands of handles.
/// - a genuinely missing or incompatible DLL, which is *not* a resource
///   condition at all and would not pass on retry.
///
/// So the note reports what is certain -- the OS refused to start the
/// process, and the "repair your build tools" advice above it is a false
/// lead -- and then lists what to check. Asserting a cause the tool has not
/// measured sends people to fix the wrong thing, which is the exact failure
/// this module exists to prevent.
pub(crate) fn process_init_failure_note(tool: &str) -> String {
    format!(
        "soldr: {tool} exited 0x{:08X} (STATUS_DLL_INIT_FAILED) -- the OS refused to \
         initialize the process, so it died before running.\n\
         soldr: this is a host process-creation failure, not a build, cache, or toolchain \
         error. Any \"repair your build tools\" advice above is a false lead.\n\
         soldr: it is usually a resource the host could not supply. Check, in order:\n\
         soldr:   1. desktop heap -- often the cause when many compilers run at once, and\n\
         soldr:      independent of handle count. Lower build parallelism to test:\n\
         soldr:        CARGO_BUILD_JOBS=4 soldr cargo ...\n\
         soldr:   2. handle exhaustion -- look for one process holding a very large count:\n\
         soldr:        Get-Process | Sort-Object HandleCount -Descending | Select-Object -First 5 Name,Id,HandleCount\n\
         soldr: the victim rotates between runs, so an identical retry often succeeds -- \
         which means a passing retry does not mean the condition is gone.",
        STATUS_DLL_INIT_FAILED as u32
    )
}

/// Print [`process_init_failure_note`] to `sink` when `exit_code` warrants it.
///
/// Returns whether the note fired, so callers can record it. Write failures
/// are ignored: this is advisory output on an already-failing path and must
/// never mask the original exit code.
pub(crate) fn report_process_init_failure(
    sink: &mut impl Write,
    tool: &str,
    exit_code: i32,
) -> bool {
    if !is_process_init_failure(exit_code) {
        return false;
    }
    let _ = writeln!(sink, "{}", process_init_failure_note(tool));
    true
}

/// Convenience wrapper over [`report_process_init_failure`] targeting stderr.
pub(crate) fn report_process_init_failure_to_stderr(tool: &str, exit_code: i32) -> bool {
    report_process_init_failure(&mut std::io::stderr(), tool, exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The whole check hinges on this constant matching what
    // `ExitStatus::code()` actually returns, which is the sign-reinterpreted
    // NTSTATUS rather than the literal 0xC0000142.
    crate::timed_test!(status_constant_is_the_sign_reinterpreted_ntstatus, {
        assert_eq!(STATUS_DLL_INIT_FAILED, -1_073_741_502);
        assert_eq!(STATUS_DLL_INIT_FAILED as u32, 0xC000_0142);
    });

    crate::timed_test!(recognizes_only_the_process_init_failure_code, {
        assert!(is_process_init_failure(STATUS_DLL_INIT_FAILED));
        // Ordinary compiler failures must not be explained away as a host
        // condition -- that would turn a real build error into a "just retry"
        // message and hide the bug.
        for code in [0, 1, 2, 101, 255, -1] {
            assert!(
                !is_process_init_failure(code),
                "exit code {code} must not be treated as process-init failure"
            );
        }
    });

    crate::timed_test!(note_names_the_tool_and_contradicts_the_toolchain_advice, {
        let note = process_init_failure_note("link.exe");
        assert!(note.contains("link.exe"), "{note}");
        assert!(note.contains("STATUS_DLL_INIT_FAILED"), "{note}");
        assert!(note.contains("0xC0000142"), "{note}");
        // The rustc-emitted "repair your build tools" line is the specific
        // false lead this exists to counter, so the rebuttal must be explicit.
        assert!(note.contains("false lead"), "{note}");
    });

    // Regression guard for the correction in this module's docs: the first
    // shipped wording asserted handle exhaustion, and 0xC0000142 was then
    // observed on a host with entirely healthy handles. Naming one unmeasured
    // cause sends people to fix the wrong thing -- the precise failure this
    // module exists to prevent -- so the note must offer the alternatives.
    crate::timed_test!(note_does_not_assert_a_single_unmeasured_cause, {
        let note = process_init_failure_note("rustc.exe");
        assert!(
            note.contains("desktop heap"),
            "desktop heap is the cause observed with healthy handles; it must be listed: {note}"
        );
        assert!(
            note.contains("HandleCount"),
            "handle exhaustion must remain one of the candidates: {note}"
        );
        assert!(
            !note.contains("(handle / paged-pool exhaustion)"),
            "must not assert a cause soldr has not measured: {note}"
        );
        // A passing retry is the trap: it looks like resolution and is not.
        assert!(
            note.contains("does not mean the condition is gone"),
            "{note}"
        );
    });

    crate::timed_test!(reports_only_on_the_matching_exit_code, {
        let mut sink = Vec::new();
        assert!(!report_process_init_failure(&mut sink, "rustc.exe", 1));
        assert!(sink.is_empty(), "a plain exit 1 must stay silent");

        let mut sink = Vec::new();
        assert!(report_process_init_failure(
            &mut sink,
            "rustc.exe",
            STATUS_DLL_INIT_FAILED
        ));
        let out = String::from_utf8(sink).expect("utf8");
        assert!(out.contains("rustc.exe"), "{out}");
    });
}
