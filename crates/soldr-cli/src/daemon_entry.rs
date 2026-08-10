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
    // The broker launches this entrypoint only after it has placed the image
    // in the route's stable runtime tree.  Never relocate or spawn from here:
    // doing so would replace the broker-owned child PID with an untracked
    // process and reopen the multi-process spawn race (soldr#2427).
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
