//! Regression guard for soldr#1667: the build session must not start
//! until every fallible pre-cargo preparation step has succeeded.
//!
//! ## What went wrong
//!
//! The cargo front door used to call `begin_build_activity_lease` — which
//! acquires the build-activity lease, sets the process-wide
//! `build_active` flag, and sends `BuildSessionStart` to the daemon —
//! *before* several preparation steps that can fail: PyO3 compatibility
//! materialization, linker override setup, cache-plan finalization and
//! application, artifact restoration, and the no-cache ownership
//! detachment.
//!
//! Any of those returning `Err` exited the function before the paired
//! end path, so the daemon kept an unfinished session record and an
//! active flag until another session or its idle timeout cleared it.
//! Daemon maintenance is suppressed while a build is active, so a
//! single rejected preflight could park maintenance indefinitely.
//!
//! ## Why a source-order lint
//!
//! The fix is an *ordering* invariant, and ordering is what regresses:
//! someone adds a new fallible setup step and reaches for the nearest
//! spot, which may be below the session start. That is invisible to a
//! behavioural test unless it injects a failure into each individual
//! step — six separate seams through an `async` function that spawns
//! cargo, for a property that is checkable directly.
//!
//! So this asserts the invariant where it actually lives: every known
//! fallible pre-spawn step appears above the session start. If you add
//! a fallible step, put it above `begin_build_activity_lease` and add it
//! to [`FALLIBLE_PRE_SPAWN_STEPS`]. If a step genuinely must run after
//! the session starts, it has to carry its own cleanup on the error
//! path, and you should say so here.
//!
//! The complementary half — that both post-cargo exit arms clear the
//! flag and emit a terminal record — is asserted below too.

use std::fs;
use std::path::{Path, PathBuf};

use crate::common;

/// Marks the start of the build session: acquires the activity lease,
/// sets `build_active`, and publishes `BuildSessionStart`.
const SESSION_START: &str = "begin_build_activity_lease(&paths, session_id";

/// Fallible preparation the front door performs before cargo runs.
/// Every one of these must appear *above* [`SESSION_START`].
///
/// Each entry is `(needle, what it is)`. The needles are deliberately
/// specific call fragments rather than bare function names, so an
/// unrelated mention in a comment does not satisfy the check.
const FALLIBLE_PRE_SPAWN_STEPS: &[(&str, &str)] = &[
    (
        "pyo3_plan.materialize_compatibility(&paths).await?",
        "PyO3 compatibility materialization",
    ),
    (
        "target::apply_linker_override(&mut command",
        "linker override setup",
    ),
    (
        "CargoCachePlan::finalize(cache_enabled_for_cargo",
        "cache-plan finalization",
    ),
    (
        "cache_plan.apply_to_command(&mut command",
        "cache-plan application",
    ),
    (
        "cache_plan.restore_rust_artifacts()?",
        "rust-artifact restoration",
    ),
    (
        "no_cache_detach::prepare_target_for_unmediated_build(",
        "no-cache ownership detachment",
    ),
];

/// Read the front-door source, or `None` when it is not on disk.
///
/// The `target-run` / `Linux x64` lanes execute a **pre-built test archive**
/// on a machine with no source tree, so a source-reading lint cannot run
/// there and skips instead of failing — the same tolerance
/// `no_timed_test_guard`'s `rust_sources` has for directories it cannot read.
///
/// soldr#2008: the path is resolved at *runtime* via `workspace_root()`.
/// `CARGO_MANIFEST_DIR` is baked in at compile time and points at whichever
/// machine built the archive, which is exactly what
/// `test_archived_source_tests_use_only_runtime_workspace_resolution`
/// forbids — and this file was violating it.
///
/// The lint still enforces everywhere it can: the `Lint` lane and any
/// local `cargo test`, both of which build and run in the checkout.
fn expand_local_includes(path: &Path) -> std::io::Result<String> {
    let text = fs::read_to_string(path)?;
    let mut expanded = String::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(relative) = trimmed
            .strip_prefix("include!(\"")
            .and_then(|rest| rest.strip_suffix("\");"))
        {
            expanded.push_str(&expand_local_includes(
                &path
                    .parent()
                    .expect("source file has a parent")
                    .join(relative),
            )?);
        } else {
            expanded.push_str(line);
            expanded.push('\n');
        }
    }
    Ok(expanded)
}

fn front_door_source() -> Option<(PathBuf, String)> {
    let path = common::workspace_root()
        .join("crates")
        .join("soldr-cli")
        .join("src")
        .join("cargo_front_door")
        .join("mod.rs");
    match expand_local_includes(&path) {
        Ok(text) => Some((path, text)),
        Err(_) => {
            eprintln!(
                "build_session_order_lint: skipping — {} is not present, so this \
                 run is executing a pre-built test archive away from the source \
                 tree. The lint enforces on the Lint lane and locally.",
                path.display(),
            );
            None
        }
    }
}

/// Line number (1-based) of the first line containing `needle`.
fn line_of(text: &str, needle: &str) -> Option<usize> {
    text.lines()
        .position(|line| line.contains(needle))
        .map(|idx| idx + 1)
}

#[test]
fn build_session_starts_after_every_fallible_setup_step() {
    let Some((path, text)) = front_door_source() else {
        return;
    };

    let start_line = line_of(&text, SESSION_START).unwrap_or_else(|| {
        panic!(
            "{}: could not find the build-session start ({SESSION_START:?}).\n\
             This lint guards soldr#1667. If the call was renamed or moved, \
             update SESSION_START in {}.",
            path.display(),
            file!(),
        )
    });

    let mut violations = Vec::new();
    for (needle, description) in FALLIBLE_PRE_SPAWN_STEPS {
        match line_of(&text, needle) {
            Some(step_line) if step_line > start_line => {
                violations.push(format!(
                    "  {description} (line {step_line}) runs AFTER the build session \
                     starts (line {start_line})"
                ));
            }
            Some(_) => {}
            None => {
                violations.push(format!(
                    "  {description}: needle {needle:?} no longer appears — the lint \
                     cannot see this step any more, so update {} rather than deleting \
                     the entry",
                    file!(),
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "soldr#1667: fallible pre-cargo setup must complete before the build \
         session starts, otherwise an Err exits before the paired \
         BuildSessionEnd and leaves the daemon holding an unfinished record \
         plus an active flag that suppresses its maintenance passes.\n\
         In {}:\n{}\n\n\
         Move the step above `begin_build_activity_lease`, or — if it genuinely \
         must run later — give it explicit cleanup on the error path and \
         document that here.",
        path.display(),
        violations.join("\n"),
    );
}

#[test]
fn both_post_session_exit_paths_clear_the_active_flag() {
    // The other half of soldr#1667: once the session *has* started, the
    // cargo-run-error arm and the normal-completion tail must each clear
    // `build_active` and emit a terminal session record. `BuildActivityLease`
    // only releases its file lock on drop — it does not touch the flag or
    // notify the daemon — so these calls cannot be dropped in favour of RAII
    // without moving that work into the guard first.
    let Some((path, text)) = front_door_source() else {
        return;
    };

    let start_line = line_of(&text, SESSION_START).expect("session start present");

    let clears: Vec<usize> = text
        .lines()
        .enumerate()
        .filter(|(_, line)| line.contains("build_active::set(false)"))
        .map(|(idx, _)| idx + 1)
        .filter(|line| *line > start_line)
        .collect();
    assert!(
        clears.len() >= 2,
        "soldr#1667: expected the cargo-error arm and the completion tail to \
         each clear `build_active` after the session starts, found {} such \
         call(s) in {}. If cleanup moved into a Drop guard, rewrite this \
         assertion to check that instead of deleting it.",
        clears.len(),
        path.display(),
    );

    let ends: Vec<usize> = text
        .lines()
        .enumerate()
        .filter(|(_, line)| line.contains("client::build_session_end("))
        .map(|(idx, _)| idx + 1)
        .filter(|line| *line > start_line)
        .collect();
    assert!(
        ends.len() >= 2,
        "soldr#1667: expected a terminal BuildSessionEnd on both the \
         cargo-error arm and the completion tail, found {} in {}.",
        ends.len(),
        path.display(),
    );
}
