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

    /// Exit as soon as this process is gone.
    ///
    /// A caller that owns a throwaway soldr root -- a test fixture with its
    /// own `SOLDR_CACHE_DIR` -- names itself here so its daemon cannot
    /// outlive it, including when the caller is killed rather than allowed
    /// to shut the daemon down. Also readable as
    /// `SOLDR_DAEMON_OWNER_PID`, which is how it reaches a daemon the broker
    /// launches, since the broker owns the argv but forwards `SOLDR_*`.
    #[arg(long, value_name = "PID")]
    owner_pid: Option<u32>,
}

/// `SOLDR_DAEMON_OWNER_PID`, the environment spelling of `--owner-pid`.
pub const OWNER_PID_ENV_VAR: &str = "SOLDR_DAEMON_OWNER_PID";

/// The flag wins over the variable: an explicit argv is a decision, while the
/// variable is inherited and may have come from an ancestor that is not the
/// intended owner. A malformed variable is ignored rather than fatal -- the
/// daemon still starts, it simply keeps its own lifetime, which is the
/// behaviour every caller had before this option existed.
fn resolve_owner_pid(flag: Option<u32>, env_value: Option<&str>) -> Option<u32> {
    flag.or_else(|| {
        env_value?
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|pid| *pid != 0)
    })
}

/// Run the daemon surface selected by the `soldr-daemon` argv[0] alias.
pub fn run() -> i32 {
    let cli = Cli::parse();
    // The broker launches this entrypoint only after it has placed the image
    // in the route's stable runtime tree.  Never relocate or spawn from here:
    // doing so would replace the broker-owned child PID with an untracked
    // process and reopen the multi-process spawn race (soldr#2427).
    let owner_env = std::env::var(OWNER_PID_ENV_VAR).ok();
    let opts = ServerOptions {
        idle_timeout: if cli.idle_timeout_secs == 0 {
            ServerOptions::default().idle_timeout
        } else {
            Duration::from_secs(cli.idle_timeout_secs)
        },
        owner_pid: resolve_owner_pid(cli.owner_pid, owner_env.as_deref()),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flag_wins_over_the_inherited_variable() {
        assert_eq!(resolve_owner_pid(Some(11), Some("22")), Some(11));
    }

    #[test]
    fn the_variable_is_used_when_no_flag_is_given() {
        assert_eq!(resolve_owner_pid(None, Some("22")), Some(22));
        assert_eq!(resolve_owner_pid(None, Some(" 22 ")), Some(22));
    }

    #[test]
    fn an_unusable_variable_leaves_the_daemon_unowned() {
        // Never fatal: an owner that cannot be parsed is the same situation
        // as no owner, and refusing to start would take out the build.
        assert_eq!(resolve_owner_pid(None, None), None);
        assert_eq!(resolve_owner_pid(None, Some("")), None);
        assert_eq!(resolve_owner_pid(None, Some("not-a-pid")), None);
        assert_eq!(resolve_owner_pid(None, Some("0")), None);
    }
}
