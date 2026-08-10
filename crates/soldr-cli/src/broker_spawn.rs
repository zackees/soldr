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
use std::time::{Duration, Instant};

/// How long the front door actively waits for a freshly-spawned broker's
/// control listener. Bounded so a wedged or slow-starting broker can never
/// turn an ordinary `soldr` invocation into a hang.
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

/// Preserve Soldr's complete identity namespace across the detached front-door
/// broker spawn. `UserBaseline` intentionally removes process-local variables;
/// without this overlay a custom `SOLDR_CACHE_DIR` reaches the wrapper but not
/// the broker or the daemon it later launches.
pub(crate) fn broker_spawn_env() -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    filter_broker_spawn_env(std::env::vars_os())
}

fn filter_broker_spawn_env(
    vars: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    vars.into_iter()
        .filter(|(name, _)| {
            name.to_string_lossy()
                .to_ascii_uppercase()
                .starts_with("SOLDR_")
        })
        .collect()
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
    true
}

/// Best-effort: confirm an existing broker or spawn one detached, then wait
/// for an active connection to its control socket. Log text is deliberately
/// not synchronization: it can be stale in the append-only spawn log and is
/// printed before the control socket is usable. A broker Hello is deliberately
/// not used either: on a registry miss, Hello is launch-capable and a mere
/// readiness check must never resurrect the daemon.
///
/// Never fails a non-compiling caller merely because eager broker startup did
/// not finish. A later cacheable compile uses the mandatory broker route and
/// reports an attributed hard failure if that route is still unavailable.
pub(crate) fn maybe_spawn_broker_front_door(raw_args: &[String]) {
    if !front_door_broker_spawn_eligible(raw_args) {
        return;
    }
    let program = broker_program();
    let Ok(probe_runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    let deadline = Instant::now() + SPAWN_WAIT_TIMEOUT;
    ensure_broker_ready_until(
        deadline,
        || broker_control_is_ready_until(&probe_runtime, &program, deadline),
        || {
            let paths = crate::core::SoldrPaths::new().ok()?;
            let log_file = open_append(&paths.root.join("broker-spawn.log"))?;
            let self_exe = std::env::current_exe().ok()?;
            let mut command = std::process::Command::new(self_exe);
            command.args(["broker", "serve", "--program", &program]);
            command.envs(broker_spawn_env());
            let stdio = daemon_stdio(&log_file);
            running_process::spawn_daemon_with_stdio_and_env_policy(
                &mut command,
                stdio,
                EnvironmentPolicy::UserBaseline,
            )
            .ok()?;
            Some(())
        },
    );
}

pub(crate) fn open_append(path: &std::path::Path) -> Option<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
}

pub(crate) fn daemon_stdio(log: &std::fs::File) -> DaemonStdio<'_> {
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

/// An accepted local-socket connection is active proof that the broker control
/// listener owns the endpoint. Drop it without sending a Hello: no service is
/// selected and the broker therefore has no request that could launch a
/// backend. This also avoids the admin client's much longer request timeout in
/// a short startup polling loop.
fn broker_control_is_ready_until(
    runtime: &tokio::runtime::Runtime,
    program: &str,
    deadline: Instant,
) -> bool {
    use running_process::broker::lifecycle::names_v2::v2_program_pipe;
    use running_process::broker::server::singleton_bind::resolve_socket_path;

    let sid = crate::broker_identity::resolve_user_sid();
    let Ok(pipe_name) = v2_program_pipe(program, &sid, 0) else {
        return false;
    };
    let Ok(endpoint) = resolve_socket_path(&pipe_name) else {
        return false;
    };
    let Ok(session_endpoint) = crate::session_transport::session_socket_path(program) else {
        return false;
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return false;
    }
    runtime.block_on(async {
        use interprocess::local_socket::tokio::prelude::*;
        use interprocess::local_socket::tokio::Stream;

        let probe = async {
            let control_name = crate::session_transport::local_session_name(&endpoint)?;
            let _control = Stream::connect(control_name).await?;
            let session_name = crate::session_transport::local_session_name(&session_endpoint)?;
            let _session = Stream::connect(session_name).await?;
            Ok::<(), std::io::Error>(())
        };
        matches!(tokio::time::timeout(remaining, probe).await, Ok(Ok(())))
    })
}

/// Probe before spawning, spawn at most once, then poll to the deadline. The
/// injected closures keep the anti-resurrection policy unit-testable without
/// binding sockets or creating processes.
fn ensure_broker_ready_until(
    deadline: Instant,
    mut ready: impl FnMut() -> bool,
    spawn: impl FnOnce() -> Option<()>,
) {
    if ready() || spawn().is_none() {
        return;
    }
    loop {
        if ready() {
            return;
        }
        if Instant::now() >= deadline {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        std::thread::sleep(POLL_INTERVAL.min(remaining));
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

    crate::timed_test!(broker_spawn_env_preserves_only_soldr_namespace, {
        use std::ffi::OsString;

        let forwarded = filter_broker_spawn_env(vec![
            (
                OsString::from("SOLDR_CACHE_DIR"),
                OsString::from("/tmp/cache"),
            ),
            (
                OsString::from("soldr_broker_program"),
                OsString::from("test-broker"),
            ),
            (OsString::from("PATH"), OsString::from("/usr/bin")),
        ]);
        assert_eq!(
            forwarded,
            vec![
                (
                    OsString::from("SOLDR_CACHE_DIR"),
                    OsString::from("/tmp/cache")
                ),
                (
                    OsString::from("soldr_broker_program"),
                    OsString::from("test-broker")
                ),
            ],
        );
    });

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

    // soldr#2388: the broker is unconditional — an ordinary invocation is
    // always eligible (there is no opt-out).
    crate::timed_test!(ordinary_invocation_is_eligible, {
        let _guard = ENV_LOCK.lock().unwrap();
        let raw_args = vec!["soldr".to_string(), "status".to_string()];
        assert!(front_door_broker_spawn_eligible(&raw_args));
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

    crate::timed_test!(ready_broker_is_not_spawned_again, {
        let mut probes = 0;
        let mut spawns = 0;
        ensure_broker_ready_until(
            Instant::now() + Duration::from_secs(1),
            || {
                probes += 1;
                true
            },
            || {
                spawns += 1;
                Some(())
            },
        );
        assert_eq!(probes, 1);
        assert_eq!(spawns, 0, "readiness must prevent a duplicate spawn");
    });

    crate::timed_test!(startup_spawns_once_then_requires_a_live_probe, {
        let mut probes = 0;
        let mut spawns = 0;
        ensure_broker_ready_until(
            Instant::now() + Duration::from_secs(1),
            || {
                probes += 1;
                probes == 3
            },
            || {
                spawns += 1;
                Some(())
            },
        );
        assert_eq!(probes, 3, "the wait must return only after a live probe");
        assert_eq!(spawns, 1, "startup may create at most one broker");
    });
}
