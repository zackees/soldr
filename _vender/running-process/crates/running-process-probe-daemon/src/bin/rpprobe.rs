//! `rpprobe` — the probe daemon's command-line client (S14 / #643).
//!
//! Thin by design: argument parsing lives in [`running_process_probe_daemon::cli`]
//! so the whole command surface is testable in-process, without spawning a
//! binary and parsing its stdout to find out whether a selection rule worked.

use clap::Parser as _;
use running_process_probe_daemon::cli::{run, Cli};

fn main() -> std::process::ExitCode {
    let code = run(Cli::parse());
    std::process::ExitCode::from(u8::try_from(code).unwrap_or(1))
}
