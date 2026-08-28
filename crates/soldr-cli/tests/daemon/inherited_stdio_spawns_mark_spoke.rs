//! Paths that hand stdio to a child must not be labelled a soldr fault
//! (soldr#2718, soldr#2726).
//!
//! `exit_guard` annotates a non-zero exit with "soldr emitted no diagnostic
//! and ran no child process ... this is a fault in soldr itself ... please
//! report it" whenever nothing called `mark_spoke()`. Its module docs ask
//! callers to mark "wherever it spawns a child that inherits stdio", because
//! that child may have done the explaining.
//!
//! Two paths have now been caught missing it, both user-visible:
//!
//! * soldr#2718 -- the Windows -> Linux Docker delegation. Every failing
//!   delegated build printed the container soldr's diagnostic and was then
//!   told to file a bug for it.
//! * soldr#2726 -- `lint deps`. It names three child processes *by pid* on
//!   the lines immediately above the annotation, then states it ran none of
//!   them.
//!
//! Why source guards rather than behavioural tests: reaching the real Docker
//! spawn needs a Windows host with a running Docker Linux engine, the
//! combination soldr#2693 records as unavailable on hosted runners. And
//! `mark_spoke()` sets a process-global one-way latch, so a same-binary test
//! asserting on `spoke()` would depend on which other tests ran first. These
//! are portable, order-independent, and catch the real regression: someone
//! deleting a call without knowing why it is there.
//!
//! This guard is deliberately a list, not a sweep. Several other files spawn
//! children without marking and are *not* defects -- they capture output
//! rather than inheriting it, or do not sit on an exit-bearing path. Entries
//! are added when a path is shown to need one, not by grepping for `spawn`.

use crate::common;

/// `(file, spawn token, what it spawns)` — every path proven to need the mark.
///
/// The spawn token is load-bearing. Checking only "does this file mention
/// `mark_spoke`" is too weak for `lint_cmd.rs`, which already marked on the
/// `lint ci` path while the `lint deps` spawn beside it did not: the exact
/// soldr#2726 bug passes such a guard. The mark has to be *near the spawn*,
/// not merely somewhere in the file.
const MUST_MARK: &[(&str, &str, &str)] = &[
    (
        "crates/soldr-cli/src/docker_cross.rs",
        "let status = tokio::process::Command::new(",
        "`docker run` for the Windows -> Linux delegation (soldr#2718)",
    ),
    (
        "crates/soldr-cli/src/lint_cmd.rs",
        "let child = command.spawn()",
        "the parallel `lint deps` children (soldr#2726)",
    ),
];

/// How many non-comment lines before the spawn the mark may sit.
///
/// Small on purpose: the point is that a reader of the spawn sees the mark.
const MARK_WINDOW_LINES: usize = 6;

#[test]
fn every_inherited_stdio_spawn_marks_the_invocation_as_having_spoken() {
    for (relative, spawn_token, what) in MUST_MARK {
        // Runtime resolution, not `CARGO_MANIFEST_DIR` -- the nextest-archive
        // replay remaps the workspace onto a different host.
        let path = common::workspace_root().join(relative);
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));

        // The rationale comments name `mark_spoke` too, so only non-comment
        // lines count as the call.
        let code: Vec<&str> = source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect();

        let spawn_line = code
            .iter()
            .position(|line| line.contains(spawn_token))
            .unwrap_or_else(|| {
                panic!(
                    "{relative} no longer contains the spawn this guard watches \
                     ({spawn_token}). If it moved or was rewritten, update this \
                     entry -- do not delete it: the mark is still required \
                     wherever {what} is spawned."
                )
            });

        let window = &code[spawn_line.saturating_sub(MARK_WINDOW_LINES)..spawn_line];
        assert!(
            window
                .iter()
                .any(|line| line.contains("exit_guard::mark_spoke()")),
            "{relative} spawns {what} with inherited stdio, so \
             `exit_guard::mark_spoke()` must appear within {MARK_WINDOW_LINES} \
             lines before the spawn. Without it, an ordinary failure on that \
             path is annotated \"soldr emitted no diagnostic and ran no child \
             process ... this is a fault in soldr itself\", telling the user to \
             file a bug for their own build. A mark elsewhere in the file does \
             not count -- that is exactly how soldr#2726 hid, with `lint ci` \
             marked and `lint deps` not."
        );
    }
}
