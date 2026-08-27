//! The CLI must be able to start (soldr#1802).
//!
//! # Why this exists
//!
//! Building soldr's clap `Command` is deeply recursive, and on Windows the
//! main thread gets 1 MiB. In a debug build that had come within *one
//! argument* of exhausting it: adding two `#[arg(long)] bool` fields to
//! `Cli` made every invocation — `soldr --version` included — die with
//! `thread 'main' has overflowed its stack` before `main` ran.
//!
//! Nothing caught it. The unit tests exercise `Cli::parse_from` inside the
//! test binary, which has its own generous stack, so they passed while the
//! shipped binary could not start at all. Only running the real executable
//! shows it.
//!
//! These assertions look trivially weak, and that is the point: the failure
//! they guard is not a wrong answer but no answer, so "exits 0 and says
//! something" is exactly the property that was lost. Keep them running the
//! **built binary** as a subprocess — a stack overflow aborts the process,
//! so it cannot be caught in-process, and an in-process check would also be
//! measuring the wrong stack.

use crate::common;

use std::process::Command;

/// Run the built soldr with `args`, returning (success, stdout+stderr).
fn run(args: &[&str]) -> (bool, String) {
    let output = Command::new(common::soldr_bin())
        .args(args)
        .output()
        .expect("spawn soldr");
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.success(), text)
}

#[test]
fn version_flag_starts_and_prints() {
    let (ok, text) = run(&["--version"]);
    assert!(
        ok,
        "`soldr --version` must exit 0. A stack overflow while clap builds \
             the command surfaces exactly here, and reports no argument and no \
             subcommand -- it reads as memory corruption rather than a CLI that \
             outgrew its stack (soldr#1802). Output was: {text}"
    );
    assert!(
        text.contains("soldr"),
        "version output must name the program: {text}"
    );
}

/// soldr#2571: the startup trace has to survive a real spawn.
///
/// The pure unit tests in `startup_trace` prove the line *shape*; only running
/// the built binary proves the marks are actually reached and that stderr
/// carries them out of the process. That is the whole point — the flake this
/// instruments produced a child with two empty streams, so "the marks emit
/// through a real spawn" is the property under test.
#[test]
fn startup_trace_names_front_door_phases_on_stderr() {
    let output = std::process::Command::new(common::soldr_bin())
        .arg("--version")
        .env(soldr_cli::startup_trace::STARTUP_TRACE_ENV_VAR, "1")
        .output()
        .expect("spawn soldr");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "`soldr --version` must still exit 0 with the trace on: {stderr}"
    );
    for phase in [
        soldr_cli::startup_trace::phase::REENTRANCY_GUARD,
        soldr_cli::startup_trace::phase::MULTICALL_DISPATCH,
    ] {
        assert!(
            stderr.contains(&format!("soldr front-door: startup phase={phase} ms=")),
            "trace must name the {phase} phase; stderr was:\n{stderr}"
        );
    }
}

/// The other half of the contract: unset means byte-for-byte the old behavior.
///
/// soldr#2554 requires `--json` / `--shell-export` payloads to stay parseable
/// when a caller merges stdout and stderr. The trace is allowed to write to
/// stderr *because* it is opt-in, so "silent unless asked" is load-bearing.
#[test]
fn startup_trace_is_silent_unless_the_env_var_asks_for_it() {
    let output = std::process::Command::new(common::soldr_bin())
        .arg("--version")
        // Removed, not set to "0": this must hold for a caller that has never
        // heard of the variable.
        .env_remove(soldr_cli::startup_trace::STARTUP_TRACE_ENV_VAR)
        .output()
        .expect("spawn soldr");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("soldr front-door: startup phase="),
        "the trace must stay off by default; stderr was:\n{stderr}"
    );
}

#[test]
fn help_flag_starts_and_prints() {
    // `--help` walks every subcommand and every global arg, so it is the
    // deepest cheap path through command construction -- the one most
    // likely to exhaust the stack first as the CLI grows.
    let (ok, text) = run(&["--help"]);
    assert!(ok, "`soldr --help` must exit 0. Output was: {text}");
    assert!(
        text.contains("Usage"),
        "help output must contain a usage line: {text}"
    );
}

/// soldr#2785: the trace must attribute the command body, not stop at parsing.
///
/// `--version` exits inside clap and never reaches `run_cli`, so the phase
/// under test cannot appear there -- this uses a real subcommand. The
/// distinction matters: a trace that ends at `clap_parse` reports the same last
/// line whether the command took 5ms or 5s, and the natural reading is that
/// startup was the cost. A `gc list` poll measured at ~278ms showed 5ms of
/// traced startup and no line accounting for the remainder.
#[test]
fn startup_trace_attributes_the_command_body() {
    let output = std::process::Command::new(common::soldr_bin())
        .arg("version")
        .env(soldr_cli::startup_trace::STARTUP_TRACE_ENV_VAR, "1")
        .output()
        .expect("spawn soldr");
    let stderr = String::from_utf8_lossy(&output.stderr);

    let dispatch = soldr_cli::startup_trace::phase::COMMAND_DISPATCH;
    assert!(
        stderr.contains(&format!("soldr front-door: startup phase={dispatch} ms=")),
        "trace must attribute the command body; stderr was:\n{stderr}"
    );

    // And it must come last: it closes the run, so anything after it would mean
    // the phase boundary is in the wrong place.
    let phases: Vec<&str> = stderr
        .lines()
        .filter(|line| line.contains("soldr front-door: startup phase="))
        .collect();
    assert!(
        phases
            .last()
            .is_some_and(|line| line.contains(&format!("phase={dispatch} "))),
        "the command body must be the final phase; trace was:\n{}",
        phases.join("\n")
    );
}

/// The flag form still exits inside clap, so it must NOT claim to have
/// dispatched a command -- otherwise the phase would be meaningless.
#[test]
fn a_clap_handled_flag_does_not_report_a_command_body() {
    let output = std::process::Command::new(common::soldr_bin())
        .arg("--version")
        .env(soldr_cli::startup_trace::STARTUP_TRACE_ENV_VAR, "1")
        .output()
        .expect("spawn soldr");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let dispatch = soldr_cli::startup_trace::phase::COMMAND_DISPATCH;
    assert!(
        !stderr.contains(&format!("phase={dispatch} ")),
        "`--version` never reaches run_cli, so it must not report a command body; stderr was:\n{stderr}"
    );
}
