#![allow(unused_imports)]

//! End-to-end proof that a rustup child which explained its own failure is
//! not re-attributed to soldr (soldr#2946, on top of soldr#2024).
//!
//! ## The failure this pins
//!
//! `soldr rustup component remove rust-src --toolchain <nightly>` produced,
//! verbatim:
//!
//! ```text
//! component target should be known
//! note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
//! soldr: exiting 101 — soldr emitted no diagnostic and ran no child process, so nothing has explained this failure.
//! soldr: this is a fault in soldr itself, not a compile error in your project; please report it with the command you ran (soldr#2024).
//! ```
//!
//! The first two lines are *rustup's own panic output*, which soldr had
//! already teed to its stderr. Two lines later soldr claimed there had been
//! no diagnostic and no child process, and sent the reader off to file a
//! soldr bug for what was a corrupted rustup component manifest. Both
//! sentences were false, and the second one is actively harmful: it
//! relabels somebody else's failure as ours.
//!
//! ## Why the unit tests are not enough
//!
//! `exit_guard`'s own tests exercise `needs_annotation(code, spoke)` — the
//! pure policy function. They cannot observe the thing that actually broke,
//! which was not the policy but the *wiring*: `run_toolchain_command` called
//! `core::run_installer_command` directly, so nothing on the rustup
//! passthrough ever called `mark_spoke()`, and `spoke` was still false at
//! the exit funnel. A test that passes `true` for `spoke` asserts the
//! conclusion the bug denied us. The only way to prove the wiring is to
//! drive a real child through the real passthrough and read what a user
//! would have read.
//!
//! ## Why this is deterministic
//!
//! No network, no real rustup, no timing. `SOLDR_TEST_RUSTUP_BIN` is the
//! first branch of `binaries::rustup_binary()`, so a fake script on disk
//! short-circuits discovery, the managed-bin lookup, and the `rustup-init`
//! auto-bootstrap entirely. `--toolchain` is passed explicitly, so
//! `scope_rustup_args_to_pin` returns early and never reads
//! `rust-toolchain.toml` — the temp workspace deliberately has none. The
//! child writes two lines and exits immediately, so the installer
//! watchdog's stall and safety timers are never in play.

use crate::common;

use crate::common::*;
use std::path::{Path, PathBuf};

/// Rust's panic exit code, and the exact status the misreported rustup
/// failure carried. Asserted as an equality rather than "non-zero" because
/// propagation and attribution are separate properties: the original bug
/// printed `exiting 101` perfectly correctly and still blamed the wrong
/// program. Remapping to 1 would be a different regression that a
/// non-zero check could not see.
const PANIC_EXIT_CODE: i32 = 101;

/// The child's own diagnostic. `run_installer_command` pipes the child's
/// stderr and tees it to soldr's, so this text reaching our stderr *is* the
/// tee working — and is what makes soldr's "no diagnostic" claim false.
const CHILD_PANIC_LINE: &str = "component target should be known";

/// The second line of a Rust panic. Present so the fixture is shaped like
/// the real report rather than a single synthetic marker.
const CHILD_BACKTRACE_NOTE: &str = "RUST_BACKTRACE=1";

/// Substrings lifted from `exit_guard::annotation`. Asserting on the exact
/// sentences (not on "soldr: " or on the absence of all output) is what
/// makes this a regression test for #2946 specifically: soldr is still free
/// to print other things, and *must* still annotate a genuinely silent
/// failure, which is #2024's guarantee and not something this test relaxes.
const NO_DIAGNOSTIC_CLAIM: &str = "emitted no diagnostic and ran no child process";
const SOLDR_FAULT_CLAIM: &str = "this is a fault in soldr itself";

/// A fake rustup that behaves like rustup panicking: two lines on stderr,
/// then exit 101.
///
/// Windows gets a `.bat` and everything else a `#!/bin/sh` script, matching
/// `install_logging_fake_rustup` and `install_doctor_fake_rustup` next door.
/// This test deliberately runs on every platform rather than early-returning
/// on `HostOs::Windows`: the exit-code path it pins is
/// `status.code().unwrap_or(1)`, and Windows is the host where "the child's
/// code" and "1" are easiest to conflate.
fn panicking_fake_rustup_script() -> String {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        format!(
            "@echo off\n\
             echo {panic_line} 1>&2\n\
             echo note: run with `{backtrace}` environment variable to display a backtrace 1>&2\n\
             exit /b {code}\n",
            panic_line = CHILD_PANIC_LINE,
            backtrace = CHILD_BACKTRACE_NOTE,
            code = PANIC_EXIT_CODE,
        )
    } else {
        // Single quotes throughout: the backtrace note contains backticks,
        // which `sh` would otherwise read as command substitution.
        format!(
            "#!/bin/sh\n\
             echo '{panic_line}' >&2\n\
             echo 'note: run with `{backtrace}` environment variable to display a backtrace' >&2\n\
             exit {code}\n",
            panic_line = CHILD_PANIC_LINE,
            backtrace = CHILD_BACKTRACE_NOTE,
            code = PANIC_EXIT_CODE,
        )
    }
}

fn install_panicking_fake_rustup() -> PathBuf {
    let dir = unique_temp_dir("fake-rustup-panicking");
    let rustup = if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        dir.join("rustup.bat")
    } else {
        fake_script_path(&dir, "rustup")
    };
    write_fake_script(&rustup, &panicking_fake_rustup_script());
    rustup
}

/// soldr#2946: drive a panicking rustup through the real `soldr rustup`
/// passthrough and assert the three properties the child boundary owes the
/// user — the child's words reach them, the child's exit code reaches them,
/// and soldr does not claim the failure as its own.
#[test]
fn a_panicking_rustup_child_is_visible_exits_101_and_is_not_blamed_on_soldr() {
    let workspace = unique_temp_dir("rustup-panicking-child");
    let rustup = install_panicking_fake_rustup();

    // `component remove rust-src --toolchain <nightly>` is the reported
    // command from #2946, verbatim. The explicit `--toolchain` also keeps
    // `scope_rustup_args_to_pin` from consulting `rust-toolchain.toml`,
    // which this workspace intentionally does not have — so the argv the
    // fake sees is fixed and the test cannot drift with the repo's pin.
    let output = isolated_soldr_command()
        .args([
            "rustup",
            "component",
            "remove",
            "rust-src",
            "--toolchain",
            "nightly-2026-01-01",
        ])
        .current_dir(&workspace)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        // `dylint_scoped_channel()` reads this before anything else and,
        // when set, rewrites the argv and injects RUSTUP_TOOLCHAIN. It is
        // exported by the dylint lanes and is not scrubbed by
        // `isolated_soldr_command`, so remove it explicitly rather than
        // letting an ambient value change what this fixture exercises.
        .env_remove("SOLDR_DYLINT_TOOLCHAIN")
        .output()
        .expect("failed to run soldr rustup component remove");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("stdout:\n{stdout}\nstderr:\n{stderr}");

    // (1) stderr visibility. `run_installer_command` pipes the child's
    // streams and tees them; without the tee the user would see an exit
    // code and nothing else, which is the #2024 failure shape.
    assert!(
        stderr.contains(CHILD_PANIC_LINE),
        "the child's stderr must reach the user through soldr's stderr — \
         that tee is the whole reason a child counts as having spoken \
         (soldr#2946)\n{combined}"
    );
    assert!(
        stderr.contains(CHILD_BACKTRACE_NOTE),
        "the child's second diagnostic line must survive the tee too; a tee \
         that truncates after one line would still pass the assertion above \
         (soldr#2946)\n{combined}"
    );

    // (2) exit-code propagation. `run_rustup_passthrough` returns
    // `status.code().unwrap_or(1)` and `guarded_exit` uses it verbatim, so
    // the caller — a CI step, a shell, `set -e` — sees rustup's own status.
    assert_eq!(
        output.status.code(),
        Some(PANIC_EXIT_CODE),
        "soldr must exit with the child's status, not remap it: 101 is \
         Rust's panic code and is how a caller tells a panicking child from \
         an ordinary error\n{combined}"
    );

    // (3) no false Soldr fault annotation. This is the assertion that fails
    // on the pre-#2946 head: `mark_spoke()` was never called on this path,
    // so `guarded_exit(101)` saw `spoke() == false` and appended both
    // sentences below directly underneath rustup's own panic.
    assert!(
        !stderr.contains(NO_DIAGNOSTIC_CLAIM),
        "soldr claimed it 'emitted no diagnostic and ran no child process' \
         immediately after teeing two lines of the child's diagnostic to \
         this very stream. Both halves of that sentence are false here \
         (soldr#2946)\n{combined}"
    );
    assert!(
        !stderr.contains(SOLDR_FAULT_CLAIM),
        "soldr must not attribute a rustup failure to itself; that sentence \
         exists for genuinely silent soldr faults (soldr#2024) and sends the \
         reader to file a soldr bug for somebody else's failure when it \
         fires here (soldr#2946)\n{combined}"
    );

    // The annotation is written as one two-line block, so a partial match
    // would mean the message changed shape without this test being updated.
    // Check stdout as well: the guarantee is about what the user sees, and
    // relocating the annotation to stdout would satisfy the checks above
    // while leaving the false claim on screen.
    assert!(
        !stdout.contains(NO_DIAGNOSTIC_CLAIM) && !stdout.contains(SOLDR_FAULT_CLAIM),
        "the false-fault annotation must not appear on stdout either — the \
         property is what reaches the user, not which stream carries it \
         (soldr#2946)\n{combined}"
    );
}
