//! `rpprobed` — probe daemon entry point.
//!
//! Thin by design: argument parsing plus a call into the library, so the
//! bring-up and discovery logic stays unit-testable without spawning a
//! process.

use std::io::Write as _;
use std::path::PathBuf;

use running_process_probe_daemon::bringup::{
    answer_identity_handshake, bring_up, BringUpConfig, Role,
};
use running_process_probe_daemon::discovery::{
    discovery_dir, generate_bearer_token, write_discovery_file, DiscoveryInfo,
};
use running_process_probe_daemon::names::{
    is_already_bound_error, probe_pipe_name, resolve_socket_path, wrap_socket_name,
};
use running_process_probe_daemon::{BEACON_PORT_ENV, EXIT_ALREADY_BOUND, EXIT_PRIVILEGED};

const USAGE: &str = "\
rpprobed — probe daemon (#631 skeleton)

USAGE:
    rpprobed [OPTIONS]

OPTIONS:
        --beacon-port <PORT>   Use exactly this beacon port instead of the
                               per-user seeded range.
        --runtime-dir <DIR>    Directory for the discovery file and Unix
                               socket. Used for test isolation.
        --elect-then-exit      Run bring-up, print the resolved role, exit.
        --linger-ms <MS>       With --elect-then-exit, how long the winner keeps
                               answering beacon handshakes before exiting
                               (default 1500). Peers racing it need this window
                               to resolve to 'client'.
    -h, --help                 Print this help.

ENVIRONMENT:
    RUNNING_PROCESS_PROBE_BEACON_PORT   Same as --beacon-port.
    RUNNING_PROCESS_BROKER_ALLOW_PRIVILEGED=1
                                        Opt out of the privilege refusal.
";

struct Args {
    beacon_port: Option<u16>,
    runtime_dir: Option<PathBuf>,
    elect_then_exit: bool,
    /// How long an --elect-then-exit winner keeps serving the beacon.
    linger_ms: u64,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        beacon_port: std::env::var(BEACON_PORT_ENV)
            .ok()
            .and_then(|v| v.parse().ok()),
        runtime_dir: None,
        elect_then_exit: false,
        linger_ms: 1_500,
    };

    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "--elect-then-exit" => args.elect_then_exit = true,
            "--linger-ms" => {
                let raw = argv.next().ok_or("--linger-ms requires a value")?;
                args.linger_ms = raw.parse().map_err(|_| format!("invalid linger: {raw}"))?;
            }
            "--beacon-port" => {
                let raw = argv.next().ok_or("--beacon-port requires a value")?;
                args.beacon_port = Some(raw.parse().map_err(|_| format!("invalid port: {raw}"))?);
            }
            "--runtime-dir" => {
                let raw = argv.next().ok_or("--runtime-dir requires a value")?;
                args.runtime_dir = Some(PathBuf::from(raw));
            }
            other => return Err(format!("unrecognized argument: {other}")),
        }
    }
    Ok(args)
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("rpprobed: {e}\n\n{USAGE}");
            std::process::exit(2);
        }
    };

    // Before any bind. A root/LocalSystem daemon would own endpoints under a
    // user-scoped name, creating a cross-user ambiguity the whole naming
    // scheme exists to prevent.
    if let Err(e) = running_process::broker::lifecycle::privilege::refuse_privileged_run() {
        eprintln!("rpprobed: refusing to run privileged: {e}");
        std::process::exit(EXIT_PRIVILEGED);
    }

    // Without the SID hash the endpoint name isn't user-scoped, so refuse
    // rather than fall back to a shared name two users could collide on.
    let sid_hash = match running_process::broker::lifecycle::sid::user_sid_hash() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("rpprobed: cannot resolve user identity for endpoint naming: {e}");
            std::process::exit(1);
        }
    };
    let cfg = BringUpConfig {
        beacon_port: args.beacon_port,
        sid_hash: sid_hash.clone(),
    };

    let role = match bring_up(&cfg) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("rpprobed: bring-up failed: {e}");
            std::process::exit(1);
        }
    };

    match role {
        Role::Client(info, port) => {
            // Another daemon owns this user's endpoints. Not an error.
            println!("role=client connected={port}");
            let _ = std::io::stdout().flush();
            if !args.elect_then_exit {
                eprintln!(
                    "rpprobed: daemon already running (pid {}); nothing to do",
                    info.daemon_pid
                );
            }
            std::process::exit(0);
        }
        Role::StrangerOnBeacon(port) => {
            // Something holds the port but isn't us. Refuse rather than
            // register with an unidentified peer.
            println!("role=stranger beacon={port}");
            let _ = std::io::stdout().flush();
            eprintln!(
                "rpprobed: a listener on port {port} failed the identity handshake; \
                 refusing to trust it"
            );
            std::process::exit(if args.elect_then_exit { 0 } else { 1 });
        }
        Role::Daemon { beacon, port } => {
            if let Err(e) = run_as_daemon(beacon, port, &sid_hash, &args) {
                eprintln!("rpprobed: {e}");
                std::process::exit(1);
            }
        }
    }
}

fn run_as_daemon(
    beacon: std::net::TcpListener,
    port: u16,
    sid_hash: &str,
    args: &Args,
) -> std::io::Result<()> {
    use interprocess::local_socket::traits::ListenerExt as _;
    use interprocess::local_socket::ListenerOptions;

    // Control socket. Single-instance enforcement rides on this bind: two
    // daemons cannot own the same name, so a losing bind means one is
    // already running.
    // Endpoint index 0 is the real per-user daemon. When --runtime-dir is set
    // the caller has asked for an isolated instance, so derive the index from
    // that path: the control socket name is machine-global (a Windows named
    // pipe especially), so without this two isolated instances would still
    // contend for one endpoint and the loser would exit "already bound" —
    // correct single-instance behavior, but it makes isolated instances
    // impossible to run concurrently.
    let endpoint_index = match args.runtime_dir.as_deref() {
        None => 0,
        Some(dir) => {
            let mut acc: u32 = 2_166_136_261;
            for b in dir.to_string_lossy().as_bytes() {
                acc ^= u32::from(*b);
                acc = acc.wrapping_mul(16_777_619);
            }
            // Keep 0 reserved for the real daemon.
            acc | 1
        }
    };
    let bare = probe_pipe_name(sid_hash, endpoint_index);
    let socket_path = resolve_socket_path(&bare);

    #[cfg(unix)]
    {
        if let Some(parent) = std::path::Path::new(&socket_path).parent() {
            std::fs::create_dir_all(parent)?;
        }
        // A Unix socket file survives a crash and would block the rebind.
        let _ = std::fs::remove_file(&socket_path);
    }

    let name = wrap_socket_name(&socket_path)?;
    let control = match ListenerOptions::new().name(name).create_sync() {
        Ok(l) => l,
        Err(e) if is_already_bound_error(&e) => {
            eprintln!("rpprobed: another instance already owns {socket_path}");
            std::process::exit(EXIT_ALREADY_BOUND);
        }
        Err(e) => return Err(e),
    };

    // Bind the HTTP listener now, but only to learn its port — serving comes
    // later. This is a plain `std` bind, which is fast; the slow parts (opening
    // the crash store, building a tokio runtime) must not run before the beacon
    // accept loop below, because peers racing this election handshake against
    // that loop and a delayed one makes them resolve to `stranger`.
    //
    // The same listener is handed to the server further down, so the port that
    // gets published is always the port that gets served.
    let http = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
    let http_port = http.local_addr()?.port();
    let bearer_token = generate_bearer_token()?;

    let info = DiscoveryInfo {
        wire_version: 1,
        control_socket: socket_path.clone(),
        http_port,
        bearer_token: bearer_token.clone(),
        daemon_pid: std::process::id(),
    };

    let discovery_target = discovery_dir(args.runtime_dir.as_deref());

    // Start before accepting registrations. A process can crash after calling
    // install but before its background registration completes; its fixed
    // spool must still be consumed by the daemon that appears later.
    let _crash_watcher = running_process_probe_daemon::crash_store::spawn_watcher(
        running_process_probe::crash::spool::spool_dir(),
        running_process_probe_daemon::crash_store::default_artifacts_dir(),
    )?;

    // Beacon accept loop: answer identity handshakes so clients can find us
    // without reading the filesystem.
    //
    // Started BEFORE announcing the role, and before any early return. A
    // winner that binds the port but never answers is worse than no daemon:
    // peers reach a listening socket, get no reply, and classify us
    // `StrangerOnBeacon`.
    let beacon_info = info.clone();
    std::thread::Builder::new()
        .name("rpprobed-beacon".into())
        .spawn(move || {
            for stream in beacon.incoming() {
                match stream {
                    Ok(mut s) => {
                        let _ = answer_identity_handshake(&mut s, &beacon_info);
                    }
                    Err(_) => break,
                }
            }
        })?;

    println!("role=daemon pid={} beacon={port}", info.daemon_pid);
    let _ = std::io::stdout().flush();

    if args.elect_then_exit {
        // Probe mode never reaches the accept loop, so publishing here is the
        // whole observable outcome of the election: a peer (and the bring-up
        // tests) needs to see that the winner published its endpoints. The
        // "publish last" ordering below exists to stop a client racing the
        // accept loop, and there is no accept loop on this path to race.
        write_discovery_file(&discovery_target, &info)?;

        // Keep serving the beacon briefly before exiting.
        //
        // Without this, a winner in probe mode releases the port the instant
        // it prints, so a peer that was racing it can then bind the same port
        // and *also* report `role=daemon` — making one election look like
        // two. Lingering lets concurrent peers complete their handshake and
        // resolve to `client`, which is the state the election actually
        // reached.
        std::thread::sleep(std::time::Duration::from_millis(args.linger_ms));
        return Ok(());
    }

    // Now the slow work, after the election has been decided and answered.
    //
    // The registration brain: shared across connections so every peer sees one
    // registry, and shared with the HTTP surface so both ingresses enforce the
    // same policy.
    let ops = running_process_probe_daemon::serve::build_ops()?;

    // HTTP surface (#642/#645): the browser UI, flame graphs, and artifact
    // downloads too large for the control socket's 16 MiB frame cap.
    let http_state = running_process_probe_daemon::http::HttpState::new(
        std::sync::Arc::clone(&ops),
        bearer_token.clone(),
    );
    match running_process_probe_daemon::http::spawn_with_listener(http, http_state) {
        Ok((bound, _handle)) => {
            // Jupyter-style: the token is unguessable, so the URL is the
            // credential. Printed once, on the daemon's own stdout, where the
            // operator who started it can see it.
            println!(
                "http=http://127.0.0.1:{}/?token={bearer_token}",
                bound.port()
            );
            let _ = std::io::stdout().flush();
        }
        // Not fatal. The control socket is the load-bearing ingress; losing the
        // UI must not take registration and capture down with it.
        Err(error) => eprintln!("rpprobed: HTTP surface unavailable: {error}"),
    }

    // Published last, immediately before the accept loop. The discovery file
    // is this daemon's "I am ready" signal, and a client that reads it goes
    // straight to the control socket — so writing it any earlier advertises a
    // daemon that is still setting up, and the client's first request lands on
    // a socket nobody is accepting on yet.
    write_discovery_file(&discovery_target, &info)?;

    // Control socket: owner-only via peer credentials. Reaching the socket is
    // not authorization — the peer must also be this user.
    for conn in control.incoming() {
        match conn {
            Ok(mut stream) => {
                let ops = std::sync::Arc::clone(&ops);
                let conn_id = running_process_probe_daemon::serve::next_conn_id();

                // Peer credentials are the authorization boundary: reaching
                // the socket is not the same as being allowed to use it.
                //
                // Read from the SOCKET, never synthesized from our own config.
                // A fabricated identity would make `dispatch`'s owner check
                // compare the owner against itself and always pass, turning
                // the defense-in-depth layer into a no-op.
                let peer = match running_process::broker::server::peer_identity_from_stream(&stream)
                {
                    Ok(peer) => peer,
                    Err(e) => {
                        // Unreadable credentials mean we cannot say who this
                        // is. Refuse rather than serve an unidentified peer.
                        eprintln!(
                            "rpprobed: refusing connection with unreadable peer credentials: {e}"
                        );
                        continue;
                    }
                };

                // One thread per connection: a slow or wedged client must not
                // stall the accept loop for everyone else.
                if let Err(e) = std::thread::Builder::new()
                    .name(format!("rpprobed-conn-{conn_id}"))
                    .spawn(move || {
                        running_process_probe_daemon::serve::serve_connection(
                            &mut stream,
                            &ops,
                            &peer,
                            conn_id,
                        );
                    })
                {
                    eprintln!("rpprobed: cannot spawn connection handler: {e}");
                }
            }
            Err(e) => {
                eprintln!("rpprobed: control accept error: {e}");
            }
        }
    }
    Ok(())
}
