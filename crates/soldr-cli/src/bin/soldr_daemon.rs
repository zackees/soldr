//! Long-lived soldr-daemon entry point. Auto-spawned by the wrapper on
//! first invocation; users rarely run this directly. Accepts a couple
//! of small flags so `soldr daemon start --foreground --idle-timeout=…`
//! can override defaults.

use clap::Parser;
use soldr_cli::daemon::server::{run, ServerError, ServerOptions};
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(name = "soldr-daemon", about = "Long-lived companion to soldr.")]
struct Cli {
    /// Run in the foreground (default). Reserved for forward
    /// compatibility — today the daemon always runs in foreground; the
    /// wrapper relies on OS-level detachment to background it.
    #[arg(long)]
    foreground: bool,

    /// Seconds of inactivity before the daemon exits. 0 disables.
    #[arg(long, value_name = "SECS", default_value_t = 1800)]
    idle_timeout_secs: u64,
}

fn main() {
    let cli = Cli::parse();
    let _ = cli.foreground;
    let opts = ServerOptions {
        idle_timeout: if cli.idle_timeout_secs == 0 {
            Duration::from_secs(u64::MAX / 2)
        } else {
            Duration::from_secs(cli.idle_timeout_secs)
        },
    };

    match run(opts) {
        Ok(()) => {}
        Err(ServerError::AlreadyRunning(pid)) => {
            eprintln!("soldr-daemon already running (pid={pid})");
            std::process::exit(0);
        }
        Err(err) => {
            eprintln!("soldr-daemon failed: {err:?}");
            std::process::exit(1);
        }
    }
}
