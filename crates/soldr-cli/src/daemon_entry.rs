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
    // This entrypoint is already the detached daemon child. Relocation must
    // replace its trampoline immediately instead of retaining a second
    // long-lived waiter process.
    crate::daemon::lifecycle::reexec_from_runtime_root(true);
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
