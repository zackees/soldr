//! Dispatch for `soldr broker <verb>` (soldr#2361 Phase 2). The front door
//! spawns `soldr broker serve` as its sole allowlisted spawn exception
//! (unconditional — soldr#2388, see `broker_spawn.rs`); manual invocation
//! remains supported and safe.
//!
//! Split out of `soldr_main.rs` on introduction rather than grown inline
//! there: that file is already over the repo's 1,500-line ratchet
//! (`.github/scripts/loc_ratchet.py`), which only allows a file already
//! over the ceiling to shrink, never grow. New dispatch surfaces belong in
//! their own module from the start, matching `daemon_entry.rs`,
//! `cache.rs`, etc.

use crate::core::SoldrError;
use crate::exit_guard::{self, guarded_exit};

/// Verbs for `soldr broker` (soldr#2361 Phase 2). Declared here rather than
/// in `cli_args.rs` (already over the repo's loc_ratchet ceiling) even
/// though `Commands::Broker` still lives there -- clap's derive works fine
/// across the module boundary as long as this type is in scope.
///
/// This is a new dispatch surface, not an argv[0] shim identity like
/// `soldr-daemon` (see `multicall.rs`'s `ShimIdentity`) -- nothing external
/// ever needs to find a broker process by a conventional binary name, only
/// soldr's own front door spawns it, so a plain subcommand is simpler than
/// installing a hardlinked shim (soldr#2364 design comment).
#[derive(clap::Subcommand)]
pub(crate) enum BrokerSubcommand {
    /// Bind the v2 broker socket and serve Hello connections, launching
    /// soldr-daemon on a verified registry miss. Blocks until the process
    /// is killed or (future work) an idle/displacement policy exits it.
    ///
    /// Spawned unconditionally by the front door (soldr#2388, see
    /// `broker_spawn.rs`); running it manually is also safe -- it enforces
    /// the same per-user-session singleton property
    /// `running-process-broker-v2` does, refusing to start a second
    /// instance rather than racing one.
    Serve {
        /// Program namespace for the bind name (advanced/testing only --
        /// distinct programs bind distinct sockets). Defaults to
        /// soldr-daemon's stable singleton program name. Overriding this
        /// requires the same `SOLDR_BROKER_PROGRAM` value on clients.
        #[arg(long, default_value = "soldr-daemon")]
        program: String,
    },
    /// Query the running broker over its control socket and print an admin
    /// status snapshot (owned routes, uptime, versions). Prints a clean
    /// "not running" line and exits 0 when no broker is bound (soldr#2442
    /// slice 2). Read-only: it never starts a broker.
    Status {
        /// Program namespace whose broker to query. Must match the value the
        /// broker was started with (defaults to the stable singleton name).
        #[arg(long, default_value = "soldr-daemon")]
        program: String,
        /// Emit the broker's JSON status payload instead of its text render.
        #[arg(long)]
        json: bool,
    },
    /// Stop the running broker and reap the daemon routes it owns (soldr#2442
    /// slice 2). Targets are taken from the broker's own admin snapshot — its
    /// self-reported PIDs — so nothing is ever killed by process name. With no
    /// broker bound it prints a "not running" line and exits 0.
    Stop {
        /// Program namespace whose broker to stop (defaults to the stable
        /// singleton name).
        #[arg(long, default_value = "soldr-daemon")]
        program: String,
    },
}

pub(crate) fn run_broker_command(command: BrokerSubcommand) -> Result<(), SoldrError> {
    match command {
        BrokerSubcommand::Serve { program } => run_broker_serve(&program),
        BrokerSubcommand::Status { program, json } => run_broker_status(&program, json),
        BrokerSubcommand::Stop { program } => run_broker_stop(&program),
    }
}

/// Bounded deadline for `broker stop` to observe graceful exits before it
/// force-kills stragglers (soldr#2442). A single constant so implementation,
/// diagnostics, and tests agree on one number; env-overridable for tests,
/// mirroring `SOLDR_SESSION_ATTEMPT_BUDGET_MS`. When the cooperative-drain
/// running-process `SHUTDOWN` verb lands (Option B), this same value travels
/// in the request as `drain_deadline_ms`.
fn broker_stop_deadline() -> std::time::Duration {
    const DEFAULT_MS: u64 = 10_000;
    let ms = std::env::var("SOLDR_BROKER_DRAIN_DEADLINE_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_MS)
        .max(1);
    std::time::Duration::from_millis(ms)
}

/// Bind the v2 broker socket for `program` and serve Hello connections,
/// launching soldr-daemon on a verified registry miss.  The requesting soldr
/// front door registers the exact source image and route before Cargo starts;
/// the broker never guesses a sibling image from its own executable.
///
/// Uses `running_process::broker::server::serve_launching_backends`, the
/// same production accept loop / launcher machinery
/// `running-process-broker-v2` is built on, so this inherits its Hello
/// validation and version-floor check for free. soldr#2442 slice 1 also mints
/// broker-internal generation identity here: the broker half at startup and
/// each route's daemon half at launch (`SoldrBackendLauncher`), injected into
/// the daemon's launch env. soldr's dumb-terminal client does not present
/// these tokens, so nothing validates them on the hot path — see the #2442
/// design ruling (the persistent SESSION pipe already signals a cycle by EOF).
fn run_broker_serve(program: &str) -> Result<(), SoldrError> {
    use fs2::FileExt;
    use running_process::broker::server::session_token::SessionTokenAuthority;
    use running_process::broker::server::{
        serve_launching_backends_with_launcher, BrokerLaunchServeConfig,
    };

    // soldr#2388: container-safe identity — the broker is mandatory for every
    // compile, so it must not hard-fail where the OS ships no /etc/machine-id.
    let socket_path = broker_control_socket_path(program)?;

    let ownership_file = broker_ownership_file(program)?;
    if let Err(err) = ownership_file.try_lock_exclusive() {
        if broker_lock_is_contended(&err) {
            eprintln!(
                "soldr broker: another broker already owns program={program}; refusing split control/SESSION ownership"
            );
            exit_guard::mark_spoke();
            guarded_exit(75);
        }
        return Err(SoldrError::Other(format!(
            "soldr broker: could not acquire ownership lock: {err}"
        )));
    }
    // Held for the complete serve lifetime. Both control and SESSION bind only
    // after this point, so concurrent starters cannot split endpoint ownership.
    let _ownership_file = ownership_file;

    // soldr#2442 slice 1: one broker/daemon generation-token authority per
    // broker process, minted here (broker half from OS randomness) and shared —
    // memory-only, never persisted — with the launcher, which mints each
    // route's daemon half at launch. Minted BEFORE the "binding at" line so it
    // stays out of the latency-sensitive window between that readiness line and
    // the control-socket bind (the window `two_brokers_against_one_program_never_coexist`
    // guards). Rotating the broker half invalidates every session across every
    // daemon at once; invalidating one daemon's half signals only that daemon's.
    let session_tokens = match SessionTokenAuthority::new() {
        Ok(authority) => std::sync::Arc::new(std::sync::Mutex::new(authority)),
        Err(err) => {
            return Err(SoldrError::Other(format!(
                "soldr broker: could not mint broker session token: {err}"
            )));
        }
    };

    println!("soldr broker: binding at {socket_path} (program={program})");

    // SESSION 0x5350 companion relay (soldr#2388 Step 7 / #2386 Option A, topology
    // (c)): a second socket serving the async full-proxy relay to the daemon's
    // deterministic SESSION endpoint, alongside the sync control serve below.
    //
    // Do ALL of the relay setup (path resolution + bind + serve) on a background
    // thread so the main thread goes straight from the "binding at" line to the
    // control-socket bind (`serve_launching_backends`). Otherwise the endpoint
    // resolution here would sit in the window between the readiness line callers
    // key on and the actual singleton bind, widening the two-broker race (this
    // regressed `two_brokers_against_one_program_never_coexist`). A failure to
    // resolve/spawn the relay is non-fatal — the control socket still serves.
    //
    // The SESSION relay does NOT validate a client-presented composite token:
    // soldr's client is a dumb terminal holding one persistent connection per
    // compile, so a broker/daemon cycle breaks that pipe directly (EOF) and
    // needs no lazy token check. The generation-token machinery is kept as
    // broker-internal identity only. See soldr#2442 and the design comment at
    // https://github.com/zackees/soldr/issues/2442#issuecomment-5246922460.
    if let Err(err) = crate::session_transport::spawn_routed_session_relay(program) {
        eprintln!("soldr broker: could not start SESSION relay ({err})");
    }

    let config = BrokerLaunchServeConfig::unbounded(socket_path.clone());
    let launcher = crate::broker_launcher::SoldrBackendLauncher::new(session_tokens);
    match serve_launching_backends_with_launcher(config, &launcher) {
        Ok(()) => Ok(()),
        Err(err) => {
            if broker_serve_error_is_already_bound(&err) {
                eprintln!(
                    "soldr broker: another broker is already bound at {socket_path} \
                     (program={program}). Refusing to start to avoid double-bind. \
                     Stop the other broker first, or pass a distinct --program."
                );
                exit_guard::mark_spoke(); // soldr#2024: this IS the explanation.
                guarded_exit(75); // EX_TEMPFAIL -- supervisor can retry after the other broker exits
            }
            Err(SoldrError::Other(format!(
                "soldr broker: serve failed: {err}"
            )))
        }
    }
}

/// Resolve the broker's control-socket path for `program` — the same
/// derivation `run_broker_serve` binds and `run_broker_status` dials, so a
/// status query always targets the exact socket the broker owns. Control is
/// pipe index 0 (the SESSION companion relay is index 1).
fn broker_control_socket_path(program: &str) -> Result<String, SoldrError> {
    use running_process::broker::lifecycle::names_v2::v2_program_pipe;
    use running_process::broker::server::singleton_bind::resolve_socket_path;

    const BROKER_PIPE_IDX: u32 = 0;

    let sid = crate::broker_identity::resolve_user_sid();
    let pipe_name = v2_program_pipe(program, &sid, BROKER_PIPE_IDX)
        .map_err(|e| SoldrError::Other(format!("soldr broker: v2_program_pipe failed: {e}")))?;
    resolve_socket_path(&pipe_name)
        .map_err(|e| SoldrError::Other(format!("soldr broker: resolve_socket_path failed: {e}")))
}

/// `soldr broker status`: send one admin STATUS request to the running broker
/// and print its reply (soldr#2442 slice 2). A missing broker is not an error
/// — it prints a "not running" line and exits 0, mirroring
/// `soldr daemon stop`'s not-running convention — so scripts can probe without
/// starting anything. Read-only: this never binds or launches a broker.
fn run_broker_status(program: &str, json: bool) -> Result<(), SoldrError> {
    use running_process::broker::client::{send_admin_request, BrokerClientError};
    use running_process::broker::protocol::{AdminRequest, AdminVerb};

    let socket_path = broker_control_socket_path(program)?;
    let request = AdminRequest {
        verb: AdminVerb::Status as i32,
        json,
        ..Default::default()
    };
    match send_admin_request(&socket_path, request) {
        Ok(reply) => {
            print!("{}", reply.body);
            if !reply.body.ends_with('\n') {
                println!();
            }
            Ok(())
        }
        // No broker bound at this socket: report cleanly and succeed, rather
        // than surfacing a transport error, so `broker status` is a safe probe.
        Err(BrokerClientError::BrokerConnect(_)) => {
            println!(
                "soldr broker: not running (no broker bound at {socket_path} for program={program})"
            );
            Ok(())
        }
        Err(err) => Err(SoldrError::Other(format!(
            "soldr broker: status query failed: {err}"
        ))),
    }
}

/// `soldr broker stop`: stop the running broker and reap the daemon routes it
/// owns (soldr#2442 slice 2). The broker's own admin STATUS snapshot is the
/// source of truth for what to terminate — its self-reported broker PID and
/// backend route PIDs — so nothing is resolved by process name (the migration
/// contract the issue's open question requires). A missing broker prints a
/// "not running" line and exits 0.
///
/// This is the verified-PID baseline (and the migration fallback for brokers
/// that predate cooperative drain). It terminates the broker first — which
/// stops new admission and releases its control + SESSION endpoints and its
/// ownership lock on exit — then reaps the daemon routes it owned, bounded by
/// [`broker_stop_deadline`], force-killing anything that overruns. In-flight
/// compiles see their SESSION pipe close (EOF): the defined cut. The
/// cooperative-drain path (the running-process `SHUTDOWN` admin verb, Option B)
/// quiesces sessions before this terminate and is the next increment.
fn run_broker_stop(program: &str) -> Result<(), SoldrError> {
    use running_process::broker::backend_lifecycle::verify_pid::{
        force_kill_pid, process_is_alive, signal_terminate,
    };
    use running_process::broker::client::{send_admin_request, BrokerClientError};
    use running_process::broker::protocol::{AdminRequest, AdminVerb};

    let socket_path = broker_control_socket_path(program)?;

    let reply = match send_admin_request(
        &socket_path,
        AdminRequest {
            verb: AdminVerb::Status as i32,
            json: true,
            ..Default::default()
        },
    ) {
        Ok(reply) => reply,
        Err(BrokerClientError::BrokerConnect(_)) => {
            println!("soldr broker: not running (nothing to stop for program={program})");
            return Ok(());
        }
        Err(err) => {
            return Err(SoldrError::Other(format!(
                "soldr broker: could not query broker before stop: {err}"
            )))
        }
    };

    let snapshot: serde_json::Value = serde_json::from_str(&reply.body).map_err(|err| {
        SoldrError::Other(format!(
            "soldr broker: could not parse broker status snapshot before stop: {err}"
        ))
    })?;
    let broker_pid = snapshot
        .get("broker_pid")
        .and_then(serde_json::Value::as_u64)
        .filter(|pid| *pid != 0)
        .map(|pid| pid as u32);
    let daemon_pids: Vec<u32> = snapshot
        .get("backends")
        .and_then(serde_json::Value::as_array)
        .map(|backends| {
            backends
                .iter()
                .filter_map(|backend| backend.get("pid").and_then(serde_json::Value::as_u64))
                .filter(|pid| *pid != 0)
                .map(|pid| pid as u32)
                .collect()
        })
        .unwrap_or_default();

    let Some(broker_pid) = broker_pid else {
        return Err(SoldrError::Other(format!(
            "soldr broker: the broker for program={program} answered status but reported no \
             pid; refusing to stop by any less-verified means"
        )));
    };

    // soldr#2442 Option B: prefer a cooperative drain. Ask the broker to shut
    // itself down gracefully — it stops admission, drains in-flight sessions as
    // the accept loop's worker scope joins, and exits. Brokers that predate the
    // SHUTDOWN verb answer exit_code 2 (or the request errors), and we fall back
    // to terminating the broker by its verified PID.
    let cooperative = try_cooperative_shutdown(&socket_path);
    if cooperative {
        println!(
            "soldr broker: requested cooperative shutdown (drain deadline {}ms)",
            broker_stop_deadline().as_millis()
        );
    }

    // The broker does not reap its own daemon routes on shutdown, so reap them
    // here regardless. Terminate the broker by PID only when cooperative drain
    // was unavailable; otherwise let it exit on its own and force-kill below
    // only if it overruns the deadline.
    let mut terminate: Vec<(&str, u32)> = Vec::new();
    if !cooperative {
        terminate.push(("broker", broker_pid));
    }
    terminate.extend(daemon_pids.iter().map(|pid| ("daemon", *pid)));
    for (kind, pid) in &terminate {
        if let Err(err) = signal_terminate(*pid) {
            eprintln!("soldr broker: could not signal {kind} pid {pid} to terminate: {err}");
        }
    }

    // Watch the broker too, so a cooperative drain that overruns the deadline is
    // force-killed rather than left running.
    let mut watch: Vec<(&str, u32)> = vec![("broker", broker_pid)];
    watch.extend(daemon_pids.iter().map(|pid| ("daemon", *pid)));
    let deadline = std::time::Instant::now() + broker_stop_deadline();
    loop {
        let alive: Vec<(&str, u32)> = watch
            .iter()
            .copied()
            .filter(|(_, pid)| process_is_alive(*pid))
            .collect();
        if alive.is_empty() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            for (kind, pid) in &alive {
                eprintln!(
                    "soldr broker: {kind} pid {pid} did not exit within the stop deadline; \
                     force-killing"
                );
                let _ = force_kill_pid(*pid);
            }
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    if daemon_pids.is_empty() {
        println!("soldr broker: stopped (broker pid {broker_pid})");
    } else {
        println!(
            "soldr broker: stopped (broker pid {broker_pid}, {} daemon route(s) reaped)",
            daemon_pids.len()
        );
    }
    Ok(())
}

/// Ask the broker to shut down cooperatively via the running-process
/// `SHUTDOWN` admin verb (soldr#2442 Option B), carrying the drain deadline.
/// Returns true if the broker acknowledged (exit_code 0); false if it does not
/// support the verb (exit_code 2) or the request failed — the caller then falls
/// back to verified-PID termination.
fn try_cooperative_shutdown(socket_path: &str) -> bool {
    use running_process::broker::client::send_admin_request;
    use running_process::broker::protocol::{AdminRequest, AdminVerb};

    let request = AdminRequest {
        verb: AdminVerb::Shutdown as i32,
        drain_deadline_ms: broker_stop_deadline().as_millis() as u64,
        ..Default::default()
    };
    matches!(
        send_admin_request(socket_path, request),
        Ok(reply) if reply.exit_code == 0
    )
}

fn broker_ownership_file(program: &str) -> Result<std::fs::File, SoldrError> {
    use sha2::{Digest, Sha256};

    let root = crate::daemon::service_definition::broker_owned_paths().root;
    std::fs::create_dir_all(&root)?;
    let digest = hex::encode(Sha256::digest(program.as_bytes()));
    Ok(std::fs::OpenOptions::new()
        .create(true)
        // This is an advisory lock file, not a data file: opening it must never
        // discard a concurrent owner's contents. `.truncate(false)` states that
        // explicitly (clippy::suspicious_open_options) and matches the prior
        // create+read+write behavior.
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(format!("broker-owner-{}.lock", &digest[..24])))?)
}

fn broker_lock_is_contended(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::WouldBlock
        || cfg!(windows) && matches!(err.raw_os_error(), Some(32 | 33))
}

/// Classify a [`running_process::broker::server::BrokerServeError`] as
/// "another broker already owns this bind path" vs any other serve
/// failure. Mirrors `running-process-broker-v2::is_already_bound_error`
/// (now `singleton_bind::is_already_bound_error`) plus the
/// `AlreadyExists` path-precheck `serve_launching_backends`'s own bind
/// step uses, which is stricter than `singleton_bind::bind_singleton`
/// (no self-healing stale-socket cleanup) -- a known follow-up to unify,
/// not a correctness issue for this dormant/opt-in first slice.
fn broker_serve_error_is_already_bound(
    err: &running_process::broker::server::BrokerServeError,
) -> bool {
    use running_process::broker::server::singleton_bind::is_already_bound_error;
    use running_process::broker::server::{
        BrokerConnectionError, BrokerServeError, ControlSocketError,
    };

    let io_err: Option<&std::io::Error> = match err {
        BrokerServeError::Connection(BrokerConnectionError::Io(e)) => Some(e),
        BrokerServeError::ControlSocket(ControlSocketError::Connection(
            BrokerConnectionError::Io(e),
        )) => Some(e),
        _ => None,
    };
    io_err
        .is_some_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists || is_already_bound_error(e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use fs2::FileExt;

    // soldr#2364: caught empirically on the Linux Docker harness -- a
    // A front-door-spawned broker under a different `--program` is unreachable
    // to the SESSION client. Lock manual invocation to the shared resolver.
    crate::timed_test!(serve_program_default_matches_daemon_service_name, {
        let command = crate::cli_args::Cli::command();
        let broker = command
            .find_subcommand("broker")
            .expect("broker subcommand registered");
        let serve = broker
            .find_subcommand("serve")
            .expect("serve subcommand registered");
        let program_arg = serve
            .get_arguments()
            .find(|arg| arg.get_id() == "program")
            .expect("program arg exists");
        let default: Vec<String> = program_arg
            .get_default_values()
            .iter()
            .map(|v| v.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            default,
            vec![crate::daemon::backend_handle_adoption::SOLDR_DAEMON_SERVICE_NAME.to_string()],
        );
    });

    crate::timed_test!(one_process_owns_control_and_session_as_a_unit, {
        let program = format!("soldr-broker-owner-test-{}", std::process::id());
        let first = broker_ownership_file(&program).expect("first ownership file");
        first.try_lock_exclusive().expect("first owner");
        let second = broker_ownership_file(&program).expect("second ownership file");
        let err = second
            .try_lock_exclusive()
            .expect_err("a second broker must not enter either bind path");
        assert!(broker_lock_is_contended(&err), "{err}");
        FileExt::unlock(&first).expect("unlock first owner");
        second
            .try_lock_exclusive()
            .expect("ownership must recover after owner exit");
        FileExt::unlock(&second).expect("unlock second owner");
    });
}
