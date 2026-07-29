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

mod common;

use std::process::Command;
use std::time::Duration;

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

soldr_cli::timed_test!(version_flag_starts_and_prints, Duration::from_secs(120), {
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
});

soldr_cli::timed_test!(help_flag_starts_and_prints, Duration::from_secs(120), {
    // `--help` walks every subcommand and every global arg, so it is the
    // deepest cheap path through command construction -- the one most
    // likely to exhaust the stack first as the CLI grows.
    let (ok, text) = run(&["--help"]);
    assert!(ok, "`soldr --help` must exit 0. Output was: {text}");
    assert!(
        text.contains("Usage"),
        "help output must contain a usage line: {text}"
    );
});
