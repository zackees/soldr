use clap::{Parser, Subcommand};

use running_process::daemon::{client, paths, server};
use running_process::proto::daemon::{StatusCode, TrackedProcess};

#[derive(Parser)]
#[command(
    name = "running-process-daemon",
    about = "Daemon for subprocess tracking"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the daemon in the background.
    ///
    /// Without any flags the daemon listens on the default global socket.
    /// Tests and parallel daemons can use --scope to derive an isolated
    /// socket/db path, or override either path directly.
    Start {
        /// Use this scope name instead of the global default. The socket
        /// and SQLite paths are derived from the scope.
        #[arg(long)]
        scope: Option<String>,
        /// Override the IPC socket path (skips scope-based derivation).
        #[arg(long)]
        socket_path: Option<String>,
        /// Override the SQLite database path (skips scope-based derivation).
        #[arg(long)]
        db_path: Option<String>,
        /// Name of the service this daemon backs.
        ///
        /// Supplied rather than derived: a daemon is parameterised by scope
        /// and has no inherent notion of which service it serves, while the
        /// broker only ever knows the service name. Passing it in is what
        /// gives the two sides a shared key (running-process#532).
        ///
        /// When set, the daemon publishes an identity file naming its IPC
        /// endpoint and removes it on exit. Omitted, nothing is published and
        /// behaviour is unchanged.
        #[arg(long)]
        service: Option<String>,
    },
    /// Stop the running daemon
    Stop,
    /// Check if the daemon is alive
    Ping,
    /// Show daemon status
    Status,
    /// List tracked processes
    List {
        #[arg(long)]
        json: bool,
        #[arg(long)]
        originator: Option<String>,
    },
    /// Find and kill zombie processes
    KillZombies {
        #[arg(long)]
        dry_run: bool,
    },
    /// Kill a specific process tree
    Kill { pid: u32 },
    /// Show process tree
    Tree { pid: u32 },
    /// Detachable PTY and pipe sessions (issue #130)
    Sessions {
        #[command(subcommand)]
        command: SessionsCommand,
    },
}

#[derive(Subcommand)]
enum SessionsCommand {
    /// List PTY and pipe sessions owned by the daemon.
    List {
        /// Filter by originator string. Empty matches all.
        #[arg(long, default_value = "")]
        originator: String,
        /// Show only PTY sessions.
        #[arg(long, conflicts_with = "pipe")]
        pty: bool,
        /// Show only pipe sessions.
        #[arg(long, conflicts_with = "pty")]
        pipe: bool,
    },
    /// Schedule termination of a session.
    Terminate {
        session_id: String,
        /// Soft signal grace window before hard kill (milliseconds).
        #[arg(long, default_value = "2000")]
        grace_ms: u32,
        /// Force pipe-session interpretation. Default: try PTY first,
        /// fall back to pipe.
        #[arg(long)]
        pipe: bool,
    },
    /// Print the current captured output of a session without attaching
    /// to it (#130 M7 B4).
    Log {
        session_id: String,
        /// For pipe sessions: which stream's backlog to dump. Ignored
        /// for PTY sessions.
        #[arg(long, value_parser = ["stdout", "stderr"], default_value = "stdout")]
        stream: String,
    },
    /// Remove exited sessions from the daemon registry (#130 M9 H4).
    Purge {
        /// Filter by originator. Empty matches all.
        #[arg(long, default_value = "")]
        originator: String,
    },
    /// Terminate every session older than a threshold (#130 M9 H4).
    /// Accepts plain seconds, or human-readable suffixes: `s`, `m`,
    /// `h`, `d` (e.g. `--older-than 1d`).
    KillOlder {
        /// Threshold age. `0` terminates everything in scope.
        #[arg(long, default_value = "0")]
        older_than: String,
        /// Filter by originator. Empty matches all.
        #[arg(long, default_value = "")]
        originator: String,
        /// Soft-signal grace window before hard kill (milliseconds).
        #[arg(long, default_value = "2000")]
        grace_ms: u32,
    },
}

fn parse_duration_secs(value: &str) -> Result<u64, String> {
    let v = value.trim();
    if v.is_empty() {
        return Err("empty duration".into());
    }
    let (digits, unit_secs) = if let Some(num) = v.strip_suffix('d') {
        (num, 86_400)
    } else if let Some(num) = v.strip_suffix('h') {
        (num, 3600)
    } else if let Some(num) = v.strip_suffix('m') {
        (num, 60)
    } else if let Some(num) = v.strip_suffix('s') {
        (num, 1)
    } else {
        (v, 1)
    };
    let n: u64 = digits
        .trim()
        .parse()
        .map_err(|e| format!("could not parse duration {value:?}: {e}"))?;
    Ok(n.saturating_mul(unit_secs))
}

/// Set the visible process/thread name for the running daemon (#222).
///
/// The daemon binary is still `running-process-daemon` (renaming it is out
/// of Phase 2 scope), but the *visible long-running identity* must be
/// `runpm-daemon`. On Linux `prctl(PR_SET_NAME)` renames the main thread,
/// which is what `ps`, `top`, and `/proc/<pid>/comm` report. On other
/// platforms this is currently a no-op; full argv0 rewriting there is
/// tracked as follow-up work.
fn set_daemon_process_name(name: &str) {
    #[cfg(target_os = "linux")]
    {
        // PR_SET_NAME truncates to 15 bytes + NUL; "runpm-daemon" fits.
        if let Ok(cstr) = std::ffi::CString::new(name) {
            // SAFETY: prctl with PR_SET_NAME and a valid NUL-terminated
            // pointer only writes the calling thread's name.
            unsafe {
                libc::prctl(libc::PR_SET_NAME, cstr.as_ptr() as libc::c_ulong, 0, 0, 0);
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = name;
    }
}

/// Initialize structured logging via `tracing-subscriber`.
///
/// Logs go to stderr (standard daemon practice).  The level is controlled by
/// the `RUST_LOG` environment variable and defaults to `info`.
fn init_logging() {
    use tracing_subscriber::EnvFilter;

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .with_thread_ids(false)
        .compact()
        .init();
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Commands::Start {
            scope,
            socket_path,
            db_path,
            service,
        } => {
            // #222: the long-running supervisor identity is `runpm-daemon`,
            // not the launcher binary name. Rename the process so it shows
            // up that way in `ps`/`top` without renaming the binary (which
            // would churn paths, spawn, and packaging across phases).
            set_daemon_process_name("runpm-daemon");
            init_logging();
            // #391: warn at startup when a systemd KillMode would reap
            // spawned children on unit stop. Silent when not under systemd.
            if let Some(warning) = running_process::systemd_killmode::probe().warning() {
                tracing::warn!("{warning}");
            }
            let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
            rt.block_on(async {
                let scope_name = scope.clone().unwrap_or_else(|| "global".to_string());
                let socket = socket_path.unwrap_or_else(|| paths::socket_path(scope.as_deref()));
                let db = db_path.unwrap_or_else(|| {
                    paths::db_path(scope.as_deref())
                        .to_string_lossy()
                        .into_owned()
                });
                let cwd = std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                let srv = match server::DaemonServer::new(
                    // Cloned because `publish_service_identity` below needs the
                    // path after the server has taken ownership. #760 added that
                    // call without this, which made the daemon binary stop
                    // compiling — invisible to CI, which builds `--features
                    // client` and never links this binary.
                    socket.clone(),
                    db,
                    scope_name,
                    scope.unwrap_or_default(),
                    cwd,
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("failed to initialize daemon: {e}");
                        std::process::exit(1);
                    }
                };
                // Publish this daemon's identity so a broker can resolve
                // `service` to a connectable endpoint. Best effort: failing to
                // publish must not stop a daemon from serving, since the IPC
                // socket — the thing clients actually use — is already bound.
                let identity =
                    service.as_deref().and_then(|service| {
                        match publish_service_identity(service, &socket) {
                            Ok(path) => {
                                println!("service identity: {}", path.display());
                                Some(path)
                            }
                            Err(e) => {
                                eprintln!("failed to publish service identity: {e}");
                                None
                            }
                        }
                    });

                let outcome = srv.run().await;

                // Retract the identity before exiting, on the error path too.
                // A stale file points a broker at a socket nobody is bound to,
                // which is worse than an absent one — absent already means
                // "no daemon", and every reader handles it.
                if let Some(path) = &identity {
                    running_process::broker::backend_sdk::remove_daemon_identity_file(path);
                }

                if let Err(e) = outcome {
                    eprintln!("daemon error: {e}");
                    std::process::exit(1);
                }
            });
        }
        Commands::Stop => match client::DaemonClient::connect(None) {
            Ok(mut c) => match c.shutdown(true, 5.0) {
                Ok(_resp) => println!("daemon is shutting down"),
                Err(e) => eprintln!("shutdown failed: {e}"),
            },
            Err(_) => eprintln!("daemon is not running"),
        },
        Commands::Ping => match client::DaemonClient::connect(None) {
            Ok(mut c) => match c.ping() {
                Ok(resp) => println!(
                    "pong (server time: {}ms)",
                    resp.ping.map(|p| p.server_time_ms).unwrap_or(0)
                ),
                Err(e) => eprintln!("ping failed: {e}"),
            },
            Err(_) => eprintln!("daemon is not running"),
        },
        Commands::Status => match client::DaemonClient::connect(None) {
            Ok(mut c) => match c.status() {
                Ok(resp) => {
                    if let Some(s) = resp.status {
                        println!("version:          {}", s.version);
                        println!("uptime:           {}s", s.uptime_seconds);
                        println!("tracked procs:    {}", s.tracked_process_count);
                        println!("active conns:     {}", s.active_connections);
                        println!("socket:           {}", s.socket_path);
                        println!("db:               {}", s.db_path);
                        if !s.scope.is_empty() {
                            println!("scope:            {}", s.scope);
                            println!("scope_hash:       {}", s.scope_hash);
                            println!("scope_cwd:        {}", s.scope_cwd);
                        }
                    } else {
                        println!("status: ok (no details)");
                    }
                }
                Err(e) => eprintln!("status failed: {e}"),
            },
            Err(_) => eprintln!("daemon is not running"),
        },
        Commands::List { json, originator } => match client::DaemonClient::connect(None) {
            Ok(mut c) => {
                let resp = if let Some(tool) = &originator {
                    c.list_by_originator(tool)
                } else {
                    c.list_active()
                };
                match resp {
                    Ok(resp) if resp.code == StatusCode::Ok as i32 => {
                        let processes = resp
                            .list_active
                            .map(|r| r.processes)
                            .or_else(|| resp.list_by_originator.map(|r| r.processes))
                            .unwrap_or_default();

                        if json {
                            print_json(&processes);
                        } else {
                            print_table(&processes);
                        }
                    }
                    Ok(resp) => eprintln!("error: {}", resp.message),
                    Err(e) => eprintln!("list failed: {e}"),
                }
            }
            Err(_) => eprintln!("daemon is not running"),
        },
        Commands::KillZombies { dry_run } => match client::DaemonClient::connect(None) {
            Ok(mut c) => match c.kill_zombies(dry_run) {
                Ok(resp) if resp.code == StatusCode::Ok as i32 => {
                    let zombies = resp.kill_zombies.map(|r| r.zombies).unwrap_or_default();
                    if zombies.is_empty() {
                        println!("no zombies found");
                    } else {
                        for z in &zombies {
                            let action = if z.killed {
                                "killed"
                            } else {
                                "found (dry-run)"
                            };
                            println!(
                                "  PID {} — {} — {} [{}]",
                                z.pid, z.command, z.reason, action
                            );
                        }
                        println!(
                            "{} zombie(s) {}",
                            zombies.len(),
                            if dry_run { "found" } else { "killed" }
                        );
                    }
                }
                Ok(resp) => eprintln!("error: {}", resp.message),
                Err(e) => eprintln!("kill-zombies failed: {e}"),
            },
            Err(_) => eprintln!("daemon is not running"),
        },
        Commands::Kill { pid } => match client::DaemonClient::connect(None) {
            Ok(mut c) => match c.kill_tree(pid, 3.0) {
                Ok(resp) if resp.code == StatusCode::Ok as i32 => {
                    let count = resp.kill_tree.map(|r| r.processes_killed).unwrap_or(0);
                    println!("killed {} process(es) in tree for PID {}", count, pid);
                }
                Ok(resp) => eprintln!("error: {}", resp.message),
                Err(e) => eprintln!("kill failed: {e}"),
            },
            Err(_) => eprintln!("daemon is not running"),
        },
        Commands::Tree { pid } => match client::DaemonClient::connect(None) {
            Ok(mut c) => match c.get_process_tree(pid) {
                Ok(resp) if resp.code == StatusCode::Ok as i32 => {
                    let display = resp
                        .get_process_tree
                        .map(|r| r.tree_display)
                        .unwrap_or_default();
                    if display.is_empty() {
                        println!("no process tree found for PID {}", pid);
                    } else {
                        println!("{}", display);
                    }
                }
                Ok(resp) => eprintln!("error: {}", resp.message),
                Err(e) => eprintln!("tree failed: {e}"),
            },
            Err(_) => eprintln!("daemon is not running"),
        },
        Commands::Sessions { command } => run_sessions_command(command),
    }
}

fn run_sessions_command(command: SessionsCommand) {
    let mut client = match client::DaemonClient::connect(None) {
        Ok(c) => c,
        Err(_) => {
            eprintln!("daemon is not running");
            std::process::exit(1);
        }
    };
    match command {
        SessionsCommand::List {
            originator,
            pty,
            pipe,
        } => {
            let show_pty = pty || !pipe;
            let show_pipe = pipe || !pty;
            if show_pty {
                match client.list_pty_sessions(&originator) {
                    Ok(sessions) => print_pty_session_table(&sessions),
                    Err(e) => eprintln!("list_pty_sessions failed: {e}"),
                }
            }
            if show_pipe {
                match client.list_pipe_sessions(&originator) {
                    Ok(sessions) => print_pipe_session_table(&sessions),
                    Err(e) => eprintln!("list_pipe_sessions failed: {e}"),
                }
            }
        }
        SessionsCommand::Purge { originator } => match client.purge_exited_sessions(&originator) {
            Ok(payload) => {
                println!(
                    "purged {} PTY + {} pipe exited sessions",
                    payload.pty_purged, payload.pipe_purged
                );
            }
            Err(e) => {
                eprintln!("purge_exited_sessions failed: {e}");
                std::process::exit(1);
            }
        },
        SessionsCommand::KillOlder {
            older_than,
            originator,
            grace_ms,
        } => {
            let secs = match parse_duration_secs(&older_than) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }
            };
            match client.bulk_terminate_sessions(secs, &originator, grace_ms) {
                Ok(payload) => {
                    println!(
                        "terminated {} PTY + {} pipe sessions older than {}s",
                        payload.pty_terminated, payload.pipe_terminated, secs
                    );
                }
                Err(e) => {
                    eprintln!("bulk_terminate_sessions failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        SessionsCommand::Log { session_id, stream } => {
            use running_process::proto::daemon::PipeStreamKind;
            let pipe_stream = match stream.as_str() {
                "stderr" => PipeStreamKind::Stderr,
                _ => PipeStreamKind::Stdout,
            };
            match client.get_session_backlog(&session_id, pipe_stream) {
                Ok(None) => {
                    eprintln!("session not found: {session_id}");
                    std::process::exit(1);
                }
                Ok(Some(payload)) => {
                    use std::io::Write;
                    if payload.bytes_missed > 0 {
                        eprintln!(
                            "[note: {} bytes dropped from the ring buffer before this snapshot]",
                            payload.bytes_missed
                        );
                    }
                    let _ = std::io::stdout().write_all(&payload.backlog);
                    if payload.exited {
                        eprintln!(
                            "[session exited with code {} at {:.3}]",
                            payload.exit_code, payload.exited_at
                        );
                    }
                }
                Err(e) => {
                    eprintln!("get_session_backlog failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        SessionsCommand::Terminate {
            session_id,
            grace_ms,
            pipe,
        } => {
            if pipe {
                match client.terminate_pipe_session(&session_id, grace_ms) {
                    Ok(()) => println!("pipe session {session_id} terminate scheduled"),
                    Err(e) => {
                        eprintln!("terminate_pipe_session failed: {e}");
                        std::process::exit(1);
                    }
                }
            } else {
                // Try PTY first, fall back to pipe on NotFound.
                match client.terminate_pty_session(&session_id, grace_ms) {
                    Ok(()) => println!("pty session {session_id} terminate scheduled"),
                    Err(client::ClientError::Server {
                        code: StatusCode::NotFound,
                        ..
                    }) => match client.terminate_pipe_session(&session_id, grace_ms) {
                        Ok(()) => {
                            println!("pipe session {session_id} terminate scheduled")
                        }
                        Err(e) => {
                            eprintln!("terminate failed: {e}");
                            std::process::exit(1);
                        }
                    },
                    Err(e) => {
                        eprintln!("terminate_pty_session failed: {e}");
                        std::process::exit(1);
                    }
                }
            }
        }
    }
}

fn print_pty_session_table(sessions: &[running_process::proto::daemon::PtySessionInfo]) {
    if sessions.is_empty() {
        println!("no PTY sessions");
        return;
    }
    println!("PTY sessions:");
    println!(
        "  {:<48} {:<7} {:<10} {:<10} COMMAND",
        "SESSION_ID", "PID", "STATE", "ATTACHED"
    );
    for s in sessions {
        let state = if s.exited {
            format!("exit({})", s.exit_code)
        } else {
            "running".into()
        };
        let attached = if s.attached { "yes" } else { "no" };
        println!(
            "  {:<48} {:<7} {:<10} {:<10} {}",
            s.session_id, s.pid, state, attached, s.command
        );
    }
}

fn print_pipe_session_table(sessions: &[running_process::proto::daemon::PipeSessionInfo]) {
    if sessions.is_empty() {
        println!("no pipe sessions");
        return;
    }
    println!("Pipe sessions:");
    println!(
        "  {:<48} {:<7} {:<10} {:<12} COMMAND",
        "SESSION_ID", "PID", "STATE", "OUT/ERR ATT"
    );
    for s in sessions {
        let state = if s.exited {
            format!("exit({})", s.exit_code)
        } else {
            "running".into()
        };
        let attached = format!(
            "{}/{}",
            if s.stdout_attached { "y" } else { "n" },
            if s.stderr_attached { "y" } else { "n" }
        );
        println!(
            "  {:<48} {:<7} {:<10} {:<12} {}",
            s.session_id, s.pid, state, attached, s.command
        );
    }
}

// ---------------------------------------------------------------------------
// Output helpers
// ---------------------------------------------------------------------------

/// Format an uptime duration in seconds as a human-readable string.
fn format_uptime(seconds: f64) -> String {
    let secs = seconds as u64;
    if secs < 60 {
        return format!("{}s", secs);
    }
    if secs < 3600 {
        return format!("{}m {}s", secs / 60, secs % 60);
    }
    format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
}

/// Map a protobuf `ProcessState` i32 value to a human-readable name.
fn state_name(state: i32) -> &'static str {
    match state {
        1 => "alive",
        2 => "dead",
        3 => "zombie",
        _ => "unknown",
    }
}

/// Print a list of tracked processes as a formatted table.
fn print_table(processes: &[TrackedProcess]) {
    if processes.is_empty() {
        println!("no tracked processes");
        return;
    }

    println!(
        "{:<8} {:<8} {:<12} {:<8} COMMAND",
        "PID", "STATE", "KIND", "UPTIME"
    );
    for p in processes {
        println!(
            "{:<8} {:<8} {:<12} {:<8} {}",
            p.pid,
            state_name(p.state),
            p.kind,
            format_uptime(p.uptime_seconds),
            p.command,
        );
    }
}

/// Print a list of tracked processes as JSON.
fn print_json(processes: &[TrackedProcess]) {
    let json_values: Vec<serde_json::Value> = processes
        .iter()
        .map(|p| {
            serde_json::json!({
                "pid": p.pid,
                "state": state_name(p.state),
                "kind": p.kind,
                "command": p.command,
                "cwd": p.cwd,
                "originator": p.originator,
                "containment": p.containment,
                "created_at": p.created_at,
                "registered_at": p.registered_at,
                "uptime_seconds": p.uptime_seconds,
                "parent_alive": p.parent_alive,
                "last_validated_at": p.last_validated_at,
            })
        })
        .collect();

    match serde_json::to_string_pretty(&json_values) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("failed to serialize JSON: {e}"),
    }
}

/// Write this daemon's identity where a broker looking up `service` will find
/// it, and return the path.
///
/// The path comes from `daemon_identity_path` rather than being built here:
/// the reader derives it from the same function, so the two cannot drift.
/// Drift would be silent — the broker would report a live daemon as absent.
fn publish_service_identity(
    service: &str,
    socket: &str,
) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    use running_process::broker::backend_lifecycle::identity::DaemonProcess;
    use running_process::broker::backend_sdk::write_daemon_identity_file;
    use running_process::broker::lifecycle::names_v2::{
        broker_v2_runtime_dir, daemon_identity_path,
    };
    use running_process::broker::protocol::Endpoint;
    use running_process::broker::secure_dir::ensure_private_dir;

    // The endpoint is recorded exactly as this daemon bound it, so a reader
    // can dial it with the same helper rather than reconstructing it.
    let endpoint = Endpoint {
        namespace_id: String::new(),
        path: socket.to_string(),
    };
    let daemon = DaemonProcess::current_process(endpoint, None)?;

    let dir = broker_v2_runtime_dir();
    ensure_private_dir(&dir)?;
    let path = daemon_identity_path(service);
    write_daemon_identity_file(&path, &daemon)?;
    Ok(path)
}
