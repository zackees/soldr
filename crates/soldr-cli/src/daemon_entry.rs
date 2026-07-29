//! `soldr-daemon` multicall entrypoint.

use crate::daemon::server::{ServerError, ServerOptions};
use clap::Parser;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(name = "soldr-daemon", about = "Long-lived companion to soldr.")]
struct Cli {
    /// Run in the foreground (the daemon is detached by its caller).
    #[arg(long)]
    foreground: bool,

    /// Seconds of inactivity before the daemon exits. 0 disables.
    #[arg(long, value_name = "SECS", default_value_t = 0)]
    idle_timeout_secs: u64,
}

/// Re-exec from the runtime root before doing anything that takes a lock.
///
/// soldr#1987. A daemon whose image is deleted out from under it keeps the
/// root-ownership lock indefinitely, and cannot be reached by `soldr daemon
/// stop`, which probes the pipe while the orphan holds the filesystem lock.
/// Relocating first means the long-lived process never pins the directory it
/// was launched from.
///
/// Every failure path here returns rather than aborting: a daemon running from
/// the wrong directory is the status quo, and strictly better than no daemon.
fn reexec_from_runtime_root() {
    let Ok(paths) = crate::core::SoldrPaths::new() else {
        return;
    };
    let Ok(current) = std::env::current_exe() else {
        return;
    };
    let Some(target) = crate::self_relocate::daemon_should_reexec(&paths, &current) else {
        return;
    };

    let mut command = std::process::Command::new(&target);
    command.args(std::env::args_os().skip(1));
    command.env(crate::self_relocate::DAEMON_REEXEC_MARKER_ENV_VAR, "1");
    eprintln!(
        "soldr-daemon: re-executing from {} so this process does not pin {}          for its lifetime (soldr#1987)",
        target.display(),
        current.display()
    );
    match command.status() {
        // The child ran the daemon to completion; inherit its result rather
        // than starting a second one here.
        Ok(status) => std::process::exit(status.code().unwrap_or(0)),
        Err(err) => {
            eprintln!(
                "soldr-daemon: could not re-exec from {}: {err}; continuing in place",
                target.display()
            );
        }
    }
}

/// Run the daemon surface selected by the `soldr-daemon` argv[0] alias.
pub fn run() -> i32 {
    reexec_from_runtime_root();
    let cli = Cli::parse();
    let _ = cli.foreground;
    let opts = ServerOptions {
        idle_timeout: if cli.idle_timeout_secs == 0 {
            ServerOptions::default().idle_timeout
        } else {
            Duration::from_secs(cli.idle_timeout_secs)
        },
    };

    match crate::daemon::server::run(opts) {
        Ok(()) => 0,
        Err(ServerError::AlreadyRunning(pid)) => {
            eprintln!("soldr-daemon already running (pid={pid})");
            0
        }
        Err(err) => {
            eprintln!("soldr-daemon failed: {err:?}");
            1
        }
    }
}
