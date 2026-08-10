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
        /// soldr-daemon's own service name -- the same string
        /// `broker_discovery::discover_via_broker` dials via
        /// `client_v2::connect`, since that API's `program` doubles as the
        /// Hello `service_name` (soldr#2364). Overriding this without also
        /// overriding `SOLDR_BROKER_PROGRAM` on the client side makes every
        /// discovery dial miss this broker.
        #[arg(long, default_value = "soldr-daemon")]
        program: String,
    },
}

pub(crate) fn run_broker_command(command: BrokerSubcommand) -> Result<(), SoldrError> {
    match command {
        BrokerSubcommand::Serve { program } => run_broker_serve(&program),
    }
}

/// Bind the v2 broker socket for `program` and serve Hello connections,
/// launching soldr-daemon on a verified registry miss. Installs (or
/// refreshes) soldr-daemon's own `.servicedef.v2` first -- the file that
/// tells the broker how -- so a freshly-started broker is immediately
/// useful without a separate `soldr daemon install-servicedef` step. If
/// that install fails, every Hello is still correctly refused as
/// `ServiceUnknown` rather than the broker guessing, matching the
/// pre-auto-install behavior.
///
/// Uses `running_process::broker::server::serve_launching_backends`, the
/// same production accept loop / launcher machinery
/// `running-process-broker-v2` is built on, so this inherits its Hello
/// validation, version-floor check, and (once soldr starts minting them --
/// separate follow-up work) composite session-token enforcement for free.
fn run_broker_serve(program: &str) -> Result<(), SoldrError> {
    use running_process::broker::lifecycle::names_v2::v2_program_pipe;
    use running_process::broker::server::singleton_bind::resolve_socket_path;
    use running_process::broker::server::{
        serve_launching_backends_with_launcher, BrokerLaunchServeConfig,
    };

    const BROKER_PIPE_IDX: u32 = 0;

    let paths = crate::core::SoldrPaths::new()?;

    // Best-effort: without this, every Hello is correctly but uselessly
    // refused as ServiceUnknown until someone remembers to run
    // `soldr daemon install-servicedef` by hand first. A failure here
    // (e.g. no sibling soldr-daemon binary next to this soldr binary in a
    // partial dev build) does not stop the broker from serving -- it just
    // stays in that same refuse-everything state, which is the existing,
    // safe fallback behavior.
    match crate::daemon::service_definition::install_default_service_definition() {
        Ok(installed) => {
            println!(
                "soldr broker: soldr-daemon servicedef installed at {}",
                installed.path.display()
            );
        }
        Err(err) => {
            eprintln!(
                "soldr broker: could not install soldr-daemon servicedef ({err}); \
                 continuing without daemon-launch capability for this service."
            );
        }
    }

    // soldr#2388: container-safe identity — the broker is mandatory for every
    // compile, so it must not hard-fail where the OS ships no /etc/machine-id.
    let sid = crate::broker_identity::resolve_user_sid();
    let pipe_name = v2_program_pipe(program, &sid, BROKER_PIPE_IDX)
        .map_err(|e| SoldrError::Other(format!("soldr broker: v2_program_pipe failed: {e}")))?;
    let socket_path = resolve_socket_path(&pipe_name)
        .map_err(|e| SoldrError::Other(format!("soldr broker: resolve_socket_path failed: {e}")))?;

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
    {
        let program = program.to_string();
        let paths = paths.clone();
        std::thread::Builder::new()
            .name("soldr-broker-session-setup".into())
            .spawn(move || {
                match crate::daemon::session_endpoint::daemon_session_endpoint_path(&paths)
                    .map_err(|e| format!("{e}"))
                {
                    Ok(backend_pipe) => {
                        if let Err(err) =
                            crate::session_transport::spawn_session_relay(&program, backend_pipe)
                        {
                            eprintln!("soldr broker: could not start SESSION relay ({err})");
                        }
                    }
                    Err(err) => eprintln!(
                        "soldr broker: could not resolve daemon SESSION endpoint ({err}); \
                         continuing without SESSION relay."
                    ),
                }
            })
            .ok();
    }

    let config = BrokerLaunchServeConfig::unbounded(socket_path.clone());
    let launcher = crate::broker_launcher::SoldrBackendLauncher::new(
        crate::broker_spawn::broker_spawn_env(),
        paths,
    );
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
    use clap::CommandFactory;

    // soldr#2364: caught empirically on the Linux Docker harness -- a
    // front-door-spawned broker bound under a *different* `--program`
    // string than `broker_discovery::discover_via_broker` dials is an
    // unreachable broker. This locks the manual-invocation default
    // (`soldr broker serve` with no `--program`) to the same string the
    // front door passes and discovery dials, so the three can never drift
    // apart silently again.
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
}
