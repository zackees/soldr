//! soldr#2361 Phase 2: the front door's "spawn the broker" allowlisted
//! exception. Opt-in via `SOLDR_USE_BROKER=1`, default OFF -- nothing calls
//! this unless the env var is set, so the default `broker-enhanced` build
//! behaves exactly as it did before this module existed.
//!
//! Per the #2364 design comment, the eventual end state deletes every
//! client-side direct-spawn path in favor of routing through the broker.
//! This slice does not do that yet -- the broker isn't consumed by anything
//! downstream, so today this is purely "does the broker come up when asked."
//! Deleting the working direct-spawn path before the broker path is proven
//! end-to-end (compile actually routed through a broker-launched daemon,
//! which needs the Linux Docker harness per this repo's Agent Development
//! Environment Rule -- not yet built) would trade a working default for a
//! non-functional one. Opt-in keeps this reversible until that proof exists.
//!
//! The one invariant that must never regress: a `RUSTC_WRAPPER` re-entry
//! (`soldr /path/to/rustc ...`, cargo calling back into soldr once per
//! compile unit) must NEVER reach this code. That path is `run_main`'s
//! `wrapper::is_wrapper_invocation` branch, which returns before this
//! module's entry point is ever called -- see the call site in
//! `soldr_main.rs`. If a broker spawn attempt fired on every compile unit
//! instead of once at the top-level invocation, it would recreate the
//! spawn-storm this whole redesign exists to kill (soldr#2360: "154x root
//! ownership is busy"). [`front_door_broker_spawn_eligible`] is a pure,
//! directly-unit-testable predicate for exactly this reason -- the call site
//! placement is a second, structural line of defense, not the only one.
//!
//! Spawns via `running_process::spawn_daemon_with_stdio_and_env_policy`
//! (the same detach machinery `soldr-daemon`'s own client-spawns-daemon path
//! uses, see `soldr_daemon::daemon::lifecycle::spawn::spawn_detached`) rather
//! than a bare `std::process::Command::spawn()`. A bare spawn on Windows
//! stays attached to the caller's job object / console, so a shell (or a
//! sandboxed tool harness) that waits for the whole descendant tree to exit
//! hangs on the long-lived broker even though the direct child (this `soldr`
//! invocation) has already returned -- caught by hand while smoke-testing
//! this against the real binary, not by any written test.

use running_process::{DaemonStdio, DaemonStdioSource, EnvironmentPolicy};
use std::io::Read;
use std::time::{Duration, Instant};

/// Opt-in gate. Default OFF -- see module doc.
const USE_BROKER_ENV_VAR: &str = "SOLDR_USE_BROKER";

/// Overrides the broker's bind namespace (default "soldr"). Test-only in
/// practice today: production has exactly one soldr broker per user
/// session, so there is normally nothing to disambiguate.
const BROKER_PROGRAM_ENV_VAR: &str = "SOLDR_BROKER_PROGRAM";

/// How long the front door waits for a freshly-spawned broker to either log
/// its "binding at" line or report an already-bound refusal. Bounded so a
/// wedged or slow-starting broker can never turn an ordinary `soldr`
/// invocation into a hang -- this whole path is best-effort, and the user's
/// actual command proceeds either way once this returns.
const SPAWN_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(50);

fn truthy_env(key: &str) -> bool {
    std::env::var(key)
        .ok()
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Pure predicate: should this top-level invocation attempt to spawn the
/// broker as its allowlisted exception? `raw_args` is the full argv
/// (`raw_args[0]` is the program name), matching `run_main`'s shape.
///
/// Kept separate from the actual spawn so the wrapper-exclusion and
/// self-recursion-exclusion rules are unit-testable without spawning a
/// process.
pub(crate) fn front_door_broker_spawn_eligible(raw_args: &[String]) -> bool {
    if !truthy_env(USE_BROKER_ENV_VAR) {
        return false;
    }
    let Some(first_positional) = raw_args.get(1) else {
        return false;
    };
    // A rustc-wrapper re-entry must never reach here -- see module doc.
    // Belt-and-suspenders: `run_main` already returns before calling this
    // module on that path, but the predicate stays correct standalone.
    if crate::wrapper::is_wrapper_invocation(first_positional) {
        return false;
    }
    // Don't spawn a broker to service a direct `soldr broker ...`
    // invocation -- that command IS the broker; spawning another one to
    // watch it start would just race its own singleton bind pointlessly.
    if first_positional == "broker" {
        return false;
    }
    true
}

/// Best-effort: spawn a detached `soldr broker serve` and wait up to
/// [`SPAWN_WAIT_TIMEOUT`] for its log to report either a successful bind or
/// an already-bound refusal (the latter also means "a broker is available",
/// since the goal is that outcome, not that this particular invocation's
/// spawn won the race). Never fails the caller's command on any outcome --
/// this is a dormant/opt-in prototype with nothing downstream consuming the
/// broker yet, so the only thing at stake here is proving the plumbing
/// works, not the user's build.
pub(crate) fn maybe_spawn_broker_front_door(raw_args: &[String]) {
    if !front_door_broker_spawn_eligible(raw_args) {
        return;
    }
    let Ok(paths) = crate::core::SoldrPaths::new() else {
        return;
    };
    let log_path = paths.root.join("broker-spawn.log");
    let Some(log_file) = open_append(&log_path) else {
        return;
    };
    let Ok(self_exe) = std::env::current_exe() else {
        return;
    };
    let program = std::env::var(BROKER_PROGRAM_ENV_VAR).unwrap_or_else(|_| "soldr".to_string());

    let mut command = std::process::Command::new(self_exe);
    command.args(["broker", "serve", "--program", &program]);
    let stdio = daemon_stdio(&log_file);
    // Best-effort: a failure to spawn just means no broker came up this
    // time, exactly like any other transient daemon-launch failure this
    // opt-in prototype doesn't yet act on.
    if running_process::spawn_daemon_with_stdio_and_env_policy(
        &mut command,
        stdio,
        EnvironmentPolicy::UserBaseline,
    )
    .is_err()
    {
        return;
    }

    wait_for_outcome(&log_path, Instant::now() + SPAWN_WAIT_TIMEOUT);
}

fn open_append(path: &std::path::Path) -> Option<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
}

fn daemon_stdio(log: &std::fs::File) -> DaemonStdio<'_> {
    #[cfg(unix)]
    {
        use std::os::fd::AsFd;
        DaemonStdio {
            stdout: DaemonStdioSource::Fd(log.as_fd()),
            stderr: DaemonStdioSource::Fd(log.as_fd()),
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsHandle;
        DaemonStdio {
            stdout: DaemonStdioSource::Handle(log.as_handle()),
            stderr: DaemonStdioSource::Handle(log.as_handle()),
        }
    }
}

/// Poll `log_path`'s tail for either broker outcome line until `deadline`.
/// Best-effort: giving up silently just leaves the broker to keep starting
/// in the background, unobserved by this invocation.
fn wait_for_outcome(log_path: &std::path::Path, deadline: Instant) {
    loop {
        if let Ok(mut file) = std::fs::File::open(log_path) {
            let mut contents = String::new();
            if file.read_to_string(&mut contents).is_ok()
                && (contents.contains("binding at") || contents.contains("already bound"))
            {
                return;
            }
        }
        if Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_use_broker(value: Option<&str>) {
        match value {
            Some(v) => std::env::set_var(USE_BROKER_ENV_VAR, v),
            None => std::env::remove_var(USE_BROKER_ENV_VAR),
        }
    }

    // soldr#2024-adjacent hazard: env::set_var/remove_var races across
    // threads within one test binary. These tests share one lock so they
    // never interleave with each other -- matches the pattern other
    // env-var-gated tests in this crate use.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn wrapper_invocation_is_never_eligible_even_with_env_set() {
        let _guard = ENV_LOCK.lock().unwrap();
        set_use_broker(Some("1"));
        let raw_args = vec!["soldr".to_string(), "/usr/bin/rustc".to_string()];
        assert!(crate::wrapper::is_wrapper_invocation(&raw_args[1]));
        assert!(!front_door_broker_spawn_eligible(&raw_args));
        set_use_broker(None);
    }

    #[test]
    fn broker_subcommand_itself_does_not_recursively_spawn() {
        let _guard = ENV_LOCK.lock().unwrap();
        set_use_broker(Some("1"));
        let raw_args = vec![
            "soldr".to_string(),
            "broker".to_string(),
            "serve".to_string(),
        ];
        assert!(!front_door_broker_spawn_eligible(&raw_args));
        set_use_broker(None);
    }

    #[test]
    fn ordinary_invocation_is_ineligible_when_env_unset() {
        let _guard = ENV_LOCK.lock().unwrap();
        set_use_broker(None);
        let raw_args = vec!["soldr".to_string(), "status".to_string()];
        assert!(!front_door_broker_spawn_eligible(&raw_args));
    }

    #[test]
    fn ordinary_invocation_is_eligible_when_env_set() {
        let _guard = ENV_LOCK.lock().unwrap();
        set_use_broker(Some("1"));
        let raw_args = vec!["soldr".to_string(), "status".to_string()];
        assert!(front_door_broker_spawn_eligible(&raw_args));
        set_use_broker(None);
    }

    #[test]
    fn no_positional_arg_is_ineligible() {
        let _guard = ENV_LOCK.lock().unwrap();
        set_use_broker(Some("1"));
        let raw_args = vec!["soldr".to_string()];
        assert!(!front_door_broker_spawn_eligible(&raw_args));
        set_use_broker(None);
    }
}
