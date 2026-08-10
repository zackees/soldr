//! soldr#2361 Phase 2: the front door's "spawn the broker" allowlisted
//! exception. **Unconditional** (soldr#2388): every eligible top-level `soldr`
//! invocation spawns/confirms the broker, and the compile hot path routes
//! through it. There is no env-var opt-out — the broker-fronted daemon is the
//! only supported topology.
//!
//! Per the #2364 design, the front door is the sole broker-spawner, and the
//! broker is the sole daemon-spawner via `serve_launching_backends`. The
//! broker→daemon→SESSION compile path is proven end-to-end on the real-process
//! integration harness (`session_multiprocess_smoke`).
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

use crate::daemon::backend_handle_adoption::broker_program;
use running_process::{DaemonStdio, DaemonStdioSource, EnvironmentPolicy};
use std::io::Read;
use std::time::{Duration, Instant};

/// How long the front door waits for a freshly-spawned broker to either log
/// its "binding at" line or report an already-bound refusal. Bounded so a
/// wedged or slow-starting broker can never turn an ordinary `soldr`
/// invocation into a hang -- this whole path is best-effort, and the user's
/// actual command proceeds either way once this returns.
const SPAWN_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Whether the broker path is enabled — **always true** (soldr#2388). The
/// broker-fronted daemon is the only supported topology: there is no env-var
/// opt-out and no legacy "direct client → daemon without a broker" mode to
/// select. Kept as a named predicate so the call sites read intentionally
/// rather than hard-coding `true`.
pub(crate) fn broker_enabled() -> bool {
    true
}

/// Pure predicate: should this top-level invocation attempt to spawn the
/// broker as its allowlisted exception? `raw_args` is the full argv
/// (`raw_args[0]` is the program name), matching `run_main`'s shape.
///
/// Kept separate from the actual spawn so the wrapper-exclusion and
/// self-recursion-exclusion rules are unit-testable without spawning a
/// process.
pub(crate) fn front_door_broker_spawn_eligible(raw_args: &[String]) -> bool {
    if !broker_enabled() {
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
    // soldr#2388: only compile-bound invocations need the broker. A command
    // that never drives a `RUSTC_WRAPPER` compile (status, version, rustfmt,
    // …) must NOT pay the broker-spawn cost. This is a DENYLIST on purpose: a
    // compile command accidentally omitted here still spawns the broker (the
    // safe default), while a non-compile command listed here merely skips a
    // spawn it never needed. Getting the denylist "too small" wastes a spawn;
    // it can never break a build by withholding the broker from a real compile.
    if NON_COMPILE_COMMANDS.contains(&first_positional.as_str()) {
        return false;
    }
    true
}

/// Top-level `soldr` subcommands that never drive a wrapper compile, so the
/// front door must not spawn a broker for them (soldr#2388). Compile-bound
/// verbs (`cargo`, `build`, `test`, `check`, `run`, `bench`, `doc`, `clippy`,
/// `fix`, `cook`, …) are deliberately absent — anything not listed spawns.
const NON_COMPILE_COMMANDS: &[&str] = &[
    "status",
    "clean",
    "config",
    "cache",
    "version",
    "help",
    "doctor",
    "wheel",
    "rustup",
    "toolchain",
    "rustfmt",
    "fmt",
    "rustdoc",
    "rust-gdb",
    "rust-lldb",
    "rust-analyzer",
    "clippy-driver",
    "logs",
    "save",
    "load",
    "archive",
    "self-update",
];

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
    let program = broker_program();

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
    use crate::daemon::backend_handle_adoption::SOLDR_BROKER_PROGRAM_ENV_VAR as BROKER_PROGRAM_ENV_VAR;

    // soldr#2024-adjacent hazard: env::set_var/remove_var races across
    // threads within one test binary. These tests share one lock so they
    // never interleave with each other -- matches the pattern other
    // env-var-gated tests in this crate use.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    crate::timed_test!(wrapper_invocation_is_never_eligible, {
        let _guard = ENV_LOCK.lock().unwrap();
        let raw_args = vec!["soldr".to_string(), "/usr/bin/rustc".to_string()];
        assert!(crate::wrapper::is_wrapper_invocation(&raw_args[1]));
        assert!(!front_door_broker_spawn_eligible(&raw_args));
    });

    crate::timed_test!(broker_subcommand_itself_does_not_recursively_spawn, {
        let _guard = ENV_LOCK.lock().unwrap();
        let raw_args = vec![
            "soldr".to_string(),
            "broker".to_string(),
            "serve".to_string(),
        ];
        assert!(!front_door_broker_spawn_eligible(&raw_args));
    });

    // soldr#2388: a compile-bound invocation is always eligible (the broker is
    // unconditional; there is no opt-out).
    crate::timed_test!(compile_invocation_is_eligible, {
        let _guard = ENV_LOCK.lock().unwrap();
        for verb in ["cargo", "build", "test", "check", "run", "clippy"] {
            let raw_args = vec!["soldr".to_string(), verb.to_string()];
            assert!(
                front_door_broker_spawn_eligible(&raw_args),
                "compile-bound `{verb}` must be broker-eligible"
            );
        }
    });

    // soldr#2388: non-compile commands must NOT pay the broker-spawn cost.
    crate::timed_test!(non_compile_invocation_is_ineligible, {
        let _guard = ENV_LOCK.lock().unwrap();
        for verb in ["status", "version", "rustfmt", "fmt", "doctor", "toolchain"] {
            let raw_args = vec!["soldr".to_string(), verb.to_string()];
            assert!(
                !front_door_broker_spawn_eligible(&raw_args),
                "non-compile `{verb}` must not spawn a broker"
            );
        }
    });

    crate::timed_test!(
        default_broker_program_matches_daemon_service_name_dialed_by_discovery,
        {
            let _guard = ENV_LOCK.lock().unwrap();
            std::env::remove_var(BROKER_PROGRAM_ENV_VAR);
            assert_eq!(
                broker_program(),
                crate::daemon::backend_handle_adoption::SOLDR_DAEMON_SERVICE_NAME,
                "the front door's broker --program must match the program \
                 client_v2::connect dials in broker_discovery, or the spawned \
                 broker is bound but unreachable (soldr#2364)",
            );
        }
    );

    crate::timed_test!(broker_program_env_override_takes_precedence, {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var(BROKER_PROGRAM_ENV_VAR, "custom-program");
        assert_eq!(broker_program(), "custom-program");
        std::env::remove_var(BROKER_PROGRAM_ENV_VAR);
    });

    crate::timed_test!(no_positional_arg_is_ineligible, {
        let _guard = ENV_LOCK.lock().unwrap();
        let raw_args = vec!["soldr".to_string()];
        assert!(!front_door_broker_spawn_eligible(&raw_args));
    });
}
