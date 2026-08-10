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
}

pub(crate) fn run_broker_command(command: BrokerSubcommand) -> Result<(), SoldrError> {
    match command {
        BrokerSubcommand::Serve { program } => run_broker_serve(&program),
    }
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
    use running_process::broker::lifecycle::names_v2::v2_program_pipe;
    use running_process::broker::server::session_token::SessionTokenAuthority;
    use running_process::broker::server::singleton_bind::resolve_socket_path;
    use running_process::broker::server::{
        serve_launching_backends_with_launcher, BrokerLaunchServeConfig,
    };

    const BROKER_PIPE_IDX: u32 = 0;

    // soldr#2388: container-safe identity — the broker is mandatory for every
    // compile, so it must not hard-fail where the OS ships no /etc/machine-id.
    let sid = crate::broker_identity::resolve_user_sid();
    let pipe_name = v2_program_pipe(program, &sid, BROKER_PIPE_IDX)
        .map_err(|e| SoldrError::Other(format!("soldr broker: v2_program_pipe failed: {e}")))?;
    let socket_path = resolve_socket_path(&pipe_name)
        .map_err(|e| SoldrError::Other(format!("soldr broker: resolve_socket_path failed: {e}")))?;

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

fn broker_ownership_file(program: &str) -> Result<std::fs::File, SoldrError> {
    use sha2::{Digest, Sha256};

    let root = crate::daemon::service_definition::broker_owned_paths().root;
    std::fs::create_dir_all(&root)?;
    let digest = hex::encode(Sha256::digest(program.as_bytes()));
    Ok(std::fs::OpenOptions::new()
        .create(true)
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
