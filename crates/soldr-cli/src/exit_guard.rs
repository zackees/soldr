//! One guaranteed line when soldr exits non-zero having explained nothing
//! (soldr#2024).
//!
//! A `soldr` invocation that exits non-zero with no output is
//! indistinguishable, from the outside, from a build that failed — by CI,
//! by `pip install`, and by the person reading the terminal. #2024 observed
//! exactly that: `soldr --no-cache cargo test …` exiting 1 having written
//! **zero bytes**, not even soldr's own unconditional no-cache preflight
//! line.
//!
//! The guarantee here is deliberately narrow, because a broad one would be
//! a lie. soldr cannot count bytes a child wrote to the inherited stdio, so
//! "did anything reach the terminal" is not knowable. What *is* knowable is
//! the two things this module tracks, and the message claims only those:
//!
//! 1. soldr routed a diagnostic through its own reporting path, and
//! 2. soldr handed its stdio to a child process, which may have spoken.
//!
//! If neither happened and the exit code is non-zero, nothing anywhere has
//! explained the failure, and [`guarded_exit`] says so. If either happened, it stays
//! quiet — so an ordinary failing build gains no noise, which is the whole
//! reason this is not simply an unconditional print at the exit funnel.
//!
//! Note what this cannot cover: a process that dies during initialisation
//! (`STATUS_DLL_INIT_FAILED`, the leading explanation for #2024's original
//! sighting) never reaches any of this. That case needs the OS-level story,
//! not a message. The value here is that it *separates* the two: with this
//! in place, a silent non-zero exit means the process really did die before
//! `main`, rather than leaving that an open question.

use std::sync::atomic::{AtomicBool, Ordering};

/// Set once soldr has either reported something itself or given a child
/// its stdio. Process-wide because the exit funnel is process-wide.
static SPOKE: AtomicBool = AtomicBool::new(false);

/// Record that this invocation has produced, or delegated, user-visible
/// output. Cheap enough to call on hot paths: one relaxed store.
///
/// Call it wherever soldr prints a diagnostic it considers the explanation
/// for a failure, and wherever it spawns a child that inherits stdio.
/// Over-calling costs only a missed warning; under-calling costs a spurious
/// one, so when in doubt, call it.
pub(crate) fn mark_spoke() {
    SPOKE.store(true, Ordering::Relaxed);
}

/// Has anything spoken for this invocation?
pub(crate) fn spoke() -> bool {
    SPOKE.load(Ordering::Relaxed)
}

/// Run a child through the installer watchdog, recording that soldr has
/// delegated its output before the child can write a word.
///
/// soldr#2946. `run_installer_command` pipes the child's stdout and stderr
/// and *tees* them to soldr's own — so a child that explains its failure has
/// explained it to the terminal, through soldr. That is precisely the
/// "delegated" case this module exists to recognise, and the four call sites
/// were not recording it. The visible cost was a rustup panic being reported
/// as soldr's fault:
///
/// ```text
/// component target should be known
/// note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
/// soldr: exiting 101 — soldr emitted no diagnostic and ran no child process, …
/// soldr: this is a fault in soldr itself, not a compile error in your project
/// ```
///
/// Two lines of rustup's own diagnostic, immediately above soldr claiming
/// there weren't any and that no child ran. The reader is then sent to file
/// a soldr bug for what was a corrupted rustup component manifest.
///
/// Marking happens *before* the spawn, not after: the module's contract is
/// that under-calling costs a spurious warning while over-calling costs only
/// a missed one, so a child that dies before writing anything should still
/// suppress the annotation — soldr did hand off, and the exit code is the
/// child's.
pub(crate) fn run_child_command(
    command: &mut std::process::Command,
    context: &str,
    phase: &str,
    config: crate::core::InstallerWatchdogConfig,
) -> Result<std::process::ExitStatus, crate::core::SoldrError> {
    mark_spoke();
    crate::core::run_installer_command(command, context, phase, config)
}

/// Should an exit with `code` be annotated, given whether anything spoke?
///
/// Split out as a pure function so the policy is unit-testable without
/// terminating the test process.
pub(crate) fn needs_annotation(code: i32, spoke: bool) -> bool {
    code != 0 && !spoke
}

/// The annotation itself. States only what is actually known — see the
/// module docs on why it does not claim "no output was produced".
pub(crate) fn annotation(code: i32) -> String {
    format!(
        "soldr: exiting {code} — soldr emitted no diagnostic and ran no child process, \
         so nothing has explained this failure.\n\
         soldr: this is a fault in soldr itself, not a compile error in your project; \
         please report it with the command you ran (soldr#2024)."
    )
}

/// Exit the process, first guaranteeing that a non-zero exit is not silent.
///
/// Replaces bare `std::process::exit` at soldr's CLI exit sites. Returns
/// `!` for the same reason `std::process::exit` does — it never comes back.
pub(crate) fn guarded_exit(code: i32) -> ! {
    if needs_annotation(code, spoke()) {
        eprintln!("{}", annotation(code));
    }
    std::process::exit(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_silent_non_zero_exit_is_annotated() {
        assert!(needs_annotation(1, false));
        assert!(needs_annotation(101, false));
        assert!(needs_annotation(-1073741502, false));
    }

    #[test]
    fn success_is_never_annotated() {
        // Nothing to explain, whether or not anything spoke.
        assert!(!needs_annotation(0, false));
        assert!(!needs_annotation(0, true));
    }

    #[test]
    fn a_failure_that_already_spoke_is_left_alone() {
        // The ordinary failing build: cargo ran and printed its own error.
        // Adding a line here would put noise on every red build, which is
        // why the guard is conditional rather than unconditional.
        assert!(!needs_annotation(1, true));
    }

    #[test]
    fn the_annotation_names_the_code_and_disclaims_a_build_error() {
        let text = annotation(1);
        assert!(text.contains("exiting 1"), "{text}");
        assert!(
            text.contains("not a compile error in your project"),
            "the message exists to stop a soldr fault reading as a broken \
             build; without that sentence it does not do its job: {text}"
        );
        assert!(text.contains("soldr#2024"), "{text}");
        // Every line must be attributable to soldr, since it lands in the
        // middle of cargo's output.
        for line in text.lines() {
            assert!(line.starts_with("soldr: "), "unprefixed line: {line:?}");
        }
    }

    #[test]
    fn mark_spoke_is_observable() {
        // The flag is process-wide and this test shares it with the rest of
        // the binary, so it may already be set; assert the transition, not
        // the initial value.
        mark_spoke();
        assert!(spoke());
    }

    #[test]
    fn a_child_that_explained_itself_is_not_blamed_on_soldr() {
        // soldr#2946. `run_installer_command` tees the child's stdout and
        // stderr to soldr's own, so a child that printed its failure has
        // explained it to the terminal *through* soldr. Before
        // `run_child_command` marked, rustup could panic with two lines of
        // its own diagnostic and soldr would still append "emitted no
        // diagnostic and ran no child process ... this is a fault in soldr
        // itself" — sending the reader to file a soldr bug for a corrupted
        // rustup component manifest.
        //
        // 101 specifically, because that is Rust's panic exit code and the
        // exact status the misreported rustup failure carried.
        assert!(!needs_annotation(101, true));
    }
}
