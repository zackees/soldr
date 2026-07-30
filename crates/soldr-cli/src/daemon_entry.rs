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

/// Run the daemon surface selected by the `soldr-daemon` argv[0] alias.
pub fn run() -> i32 {
    let cli = Cli::parse();
    // Managed startup reaches this entrypoint through running-process's
    // detached-daemon boundary, which marks the child explicitly. Relocation
    // may detach its replacement in that case so the trampoline exits instead
    // of becoming a second long-lived waiter. A direct foreground invocation
    // has no marker and must preserve its terminal/stdout/stderr contract by
    // waiting for the relocated child (soldr#2037).
    crate::daemon::lifecycle::reexec_from_runtime_root_for_daemon_entry();
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
