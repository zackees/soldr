//! v2 broker binary — production accept loop + ServiceDefinitionLoader
//! integration (running-process#532 slice 1).
//!
//! Replaces the slice-3c scaffold (`SCAFFOLD_PROGRAM` + single
//! `accept()` + exit) with a real broker:
//!
//! 1. **`--program <name>`** CLI arg names the v2 pipe namespace
//!    (`rpb-v2-<program>-<sid_hash>-0`). Defaults to
//!    `broker-v2-scaffold` for backwards compatibility with the
//!    earlier integration tests.
//! 2. **Persistent accept loop** — each accepted connection spawns
//!    a thread that handles the Hello round-trip. The accept loop
//!    polls with bounded latency so a shutdown request can be observed:
//!    SIGTERM/SIGINT on Unix, console control events on Windows.
//!    In-flight handlers are then drained with a deadline.
//! 3. **ServiceDefinitionLoader integration** — on each Hello, look
//!    up `hello.service_name` via the default v2 service-definition
//!    directory ([`ServiceDefinitionLoader::default_root`]). Reject
//!    unknown services with `ErrorServiceUnknown`; reject
//!    out-of-policy versions with `ErrorVersionBlocked` (mirrors
//!    v1's `hello_router::refused_from_version_policy`).
//! 4. **Backend-pipe resolution** — a successful Hello replies `Negotiated`
//!    carrying the IPC endpoint published by the daemon started with
//!    `--service <name>`, read from its identity file (#532 item 5). When no
//!    daemon has published, the pipe is empty rather than the Hello being
//!    refused: the service can be registered and version-compatible while its
//!    daemon has not started yet.
//!
//!    Forwarding the adopt traffic over that pipe is still future work; this
//!    resolves the endpoint, it does not proxy to it.
//!
//! Flags:
//! - `--no-bind`: skip the bind entirely; exit 0 (kept for the
//!   slice-3c integration test).
//! - `--once`: accept exactly one connection then exit (testing
//!   convenience; the persistent loop is the default).
//! - `--program <name>`: name the v2 pipe namespace. Default
//!   `broker-v2-scaffold`.
//!
//! Future slices:
//! - Adopt forwarding. Backend-pipe *resolution* has landed (#532 item 5);
//!   forwarding the adopt traffic itself has not.
//!
//! The single-instance lock and the refuse-privileged-run guard were also
//! listed here as future work; both have since landed (`is_already_bound_error`
//! and `refuse_privileged_run` respectively).

use std::env;
use std::io::Write;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use interprocess::local_socket::traits::{Listener as _, Stream as _};
use interprocess::local_socket::{ListenerNonblockingMode, ListenerOptions};
use prost::Message;
use running_process::broker::broker_http_discovery;
use running_process::broker::broker_http_port::BrokerHttpPort;
use running_process::broker::broker_http_server::BrokerHttpServer;
use running_process::broker::http_endpoint_registry::HttpEndpointRegistry;
use running_process::broker::lifecycle::names_v2::{broker_v2_runtime_dir, v2_program_pipe};
use running_process::broker::lifecycle::privilege::refuse_privileged_run;
use running_process::broker::lifecycle::sid::user_sid_hash;
use running_process::broker::protocol::{
    hello_reply, read_frame, write_frame, ErrorCode, Hello, HelloReply, Negotiated, Refused,
    ENVELOPE_VERSION,
};
use running_process::broker::protocol_v2::ServiceDefinitionLoader;
use running_process::broker::server::deadline_stream::{hello_read_deadline, DeadlineStream};
use running_process::broker::server::service_def_loader::ServiceDefinitionError;

/// Default program name when `--program` is not passed. Matches the
/// slice-3c scaffold so existing integration tests keep working.
const DEFAULT_PROGRAM: &str = "broker-v2-scaffold";
const SCAFFOLD_PIPE_IDX: u32 = 0;

/// Maximum in-flight Hello handlers. Conservative cap; the OS thread
/// cap is the hard upper bound but we want backpressure before that.
const MAX_INFLIGHT_HANDLERS: usize = 256;
const MAX_INFLIGHT_HANDLERS_ENV: &str = "RUNNING_PROCESS_BROKER_MAX_INFLIGHT_HANDLERS";
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(20);
const HANDLER_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(unix)]
static SIGNAL_SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn record_shutdown_signal(_signal: libc::c_int) {
    // Async-signal-safe by construction: do not allocate, log, join threads,
    // or call the LLVM profile runtime from this handler.
    SIGNAL_SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
}

#[cfg(unix)]
fn install_shutdown_signal_handlers() -> std::io::Result<()> {
    SIGNAL_SHUTDOWN_REQUESTED.store(false, Ordering::Relaxed);
    for signal in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: `record_shutdown_signal` has C ABI, lives for the process
        // lifetime, and performs only an atomic store.
        let previous = unsafe {
            libc::signal(
                signal,
                record_shutdown_signal as *const () as libc::sighandler_t,
            )
        };
        if previous == libc::SIG_ERR {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Set when Windows delivers a console control event.
///
/// The Windows counterpart of [`SIGNAL_SHUTDOWN_REQUESTED`]. Without it the
/// accept loop on Windows polled a flag nothing ever set, so the broker never
/// drained or unbound — it only ever died when killed.
#[cfg(windows)]
static CONSOLE_SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Console control handler.
///
/// Windows runs this on a thread it injects into the process, so it does the
/// same thing the Unix signal handler does and no more: one atomic store. No
/// allocation, no logging, no joining threads.
///
/// Returning `TRUE` claims the event. For `CTRL_C_EVENT` and
/// `CTRL_BREAK_EVENT` that suppresses the default terminate, which is the
/// point — the accept loop needs to observe the flag and drain.
#[cfg(windows)]
unsafe extern "system" fn console_ctrl_handler(
    ctrl_type: winapi::shared::minwindef::DWORD,
) -> winapi::shared::minwindef::BOOL {
    use winapi::um::wincon::{
        CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
    };

    match ctrl_type {
        CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT
        | CTRL_SHUTDOWN_EVENT => {
            CONSOLE_SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
            winapi::shared::minwindef::TRUE
        }
        // Anything else is left to the next handler rather than swallowed.
        _ => winapi::shared::minwindef::FALSE,
    }
}

/// Install the console control handler.
///
/// # The close/logoff/shutdown events are on a clock
///
/// `CTRL_C_EVENT` and `CTRL_BREAK_EVENT` leave the process running once
/// claimed. The other three do not: Windows gives a console process a few
/// seconds after `CTRL_CLOSE_EVENT` (and less at logoff/shutdown) and then
/// terminates it regardless of what the handler returns.
///
/// [`HANDLER_DRAIN_TIMEOUT`] is 5s, which is the same order as that budget —
/// so on a window close a long-running handler may be cut off mid-drain. That
/// is still strictly better than today's behavior, where the loop never even
/// begins to drain, but it is a bound rather than a guarantee and should not
/// be described as a graceful shutdown in every case.
#[cfg(windows)]
fn install_shutdown_console_handler() -> std::io::Result<()> {
    CONSOLE_SHUTDOWN_REQUESTED.store(false, Ordering::Relaxed);
    // SAFETY: `console_ctrl_handler` has the required `system` ABI, lives for
    // the process lifetime, and performs only an atomic store.
    let installed = unsafe {
        winapi::um::consoleapi::SetConsoleCtrlHandler(
            Some(console_ctrl_handler),
            winapi::shared::minwindef::TRUE,
        )
    };
    if installed == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(coverage)]
unsafe extern "C" {
    fn __llvm_profile_write_file() -> libc::c_int;
}

fn flush_coverage_profile() -> Result<(), String> {
    #[cfg(coverage)]
    {
        // SAFETY: cargo-llvm-cov links this process with LLVM's profiling
        // runtime. This call occurs after the signal handler has returned and
        // after handler threads have been drained, in ordinary Rust control
        // flow.
        let result = unsafe { __llvm_profile_write_file() };
        if result != 0 {
            return Err(format!(
                "__llvm_profile_write_file returned nonzero status {result}"
            ));
        }
    }
    Ok(())
}

fn max_inflight_handlers() -> usize {
    std::env::var(MAX_INFLIGHT_HANDLERS_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|&limit| limit > 0)
        .unwrap_or(MAX_INFLIGHT_HANDLERS)
}

struct InflightGuard(Arc<AtomicUsize>);

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Debug, Clone)]
struct CliOptions {
    no_bind: bool,
    once: bool,
    program: String,
    /// `None` means no HTTP surface at all — the default.
    ///
    /// #483 calls the per-backend HTTP servers optional, and a broker that
    /// opened a listening TCP port merely by starting would be a surprise on
    /// a shared host. Opting in keeps the default surface to the local socket.
    http_port: Option<BrokerHttpPort>,
}

/// Parse the `--http-port` value.
///
/// `dynamic` maps to an OS-allocated port. A number maps to
/// [`BrokerHttpPort::StaticOrFallback`] rather than `Static`: now that the
/// resolved port is published for discovery, falling back to an OS-allocated
/// port keeps the broker running and still reachable, whereas `Static` would
/// abort startup because something unrelated held the port. An operator who
/// truly needs an exact port can set `RUNNING_PROCESS_BROKER_HTTP_PORT`,
/// which resolution collapses to `Static`.
fn parse_http_port(value: &str) -> Result<BrokerHttpPort, String> {
    if value.eq_ignore_ascii_case("dynamic") {
        return Ok(BrokerHttpPort::Dynamic);
    }
    match value.parse::<u16>() {
        // 0 already means "OS-allocated" to the sockets layer; naming it
        // `Dynamic` keeps the resolved config honest about what will happen.
        Ok(0) => Ok(BrokerHttpPort::Dynamic),
        Ok(preferred) => Ok(BrokerHttpPort::StaticOrFallback { preferred }),
        Err(_) => Err(format!(
            "--http-port expects a port number or `dynamic`, got {value:?}"
        )),
    }
}

fn parse_cli(args: &[String]) -> Result<CliOptions, String> {
    let mut opts = CliOptions {
        no_bind: false,
        once: false,
        program: DEFAULT_PROGRAM.to_owned(),
        http_port: None,
    };
    let mut i = 1; // skip argv[0]
    while i < args.len() {
        match args[i].as_str() {
            "--no-bind" => opts.no_bind = true,
            "--once" => opts.once = true,
            "--program" => {
                i += 1;
                if i >= args.len() {
                    return Err("--program requires a value".to_owned());
                }
                opts.program = args[i].clone();
            }
            "--http-port" => {
                i += 1;
                if i >= args.len() {
                    return Err("--http-port requires a value".to_owned());
                }
                opts.http_port = Some(parse_http_port(&args[i])?);
            }
            "--help" | "-h" => {
                return Err(format!(
                    "running-process-broker-v2 {} — usage:\n  \
                     [--program <name>]     (default: {DEFAULT_PROGRAM})\n  \
                     [--once]               (accept one connection then exit)\n  \
                     [--no-bind]            (exit 0 immediately; for integration test)\n  \
                     [--http-port <n|dynamic>]  (serve the aggregation page; off by default)",
                    env!("CARGO_PKG_VERSION")
                ));
            }
            unknown => return Err(format!("unknown argument: {unknown}")),
        }
        i += 1;
    }
    Ok(opts)
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let opts = match parse_cli(&args) {
        Ok(o) => o,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };

    println!(
        "running-process-broker-v2 {} (slice 1 of running-process#532)",
        env!("CARGO_PKG_VERSION")
    );

    if opts.no_bind {
        println!("running-process-broker-v2 --no-bind: skipping listener bind");
        return ExitCode::SUCCESS;
    }

    // Slice 3 of #532: refuse to start as a privileged user. The
    // broker is a per-user daemon — running as root / LocalSystem
    // would bind the v2 pipe in a namespace other users can't reach
    // AND would create privileged sockets that downstream daemons
    // get adopted into. Mirrors v1's `running-process-broker-v1`
    // startup check exactly. The `RUNNING_PROCESS_BROKER_ALLOW_PRIVILEGED`
    // env var is honored for isolated test environments that
    // intentionally exercise privileged startup behavior.
    if let Err(err) = refuse_privileged_run() {
        eprintln!(
            "running-process-broker-v2: refusing privileged startup: {err}. \
             Run as an unprivileged user, or set \
             RUNNING_PROCESS_BROKER_ALLOW_PRIVILEGED=1 for isolated test environments only."
        );
        return ExitCode::from(77); // EX_NOPERM
    }

    let sid = match user_sid_hash() {
        Ok(s) => s,
        Err(err) => {
            eprintln!("running-process-broker-v2: user_sid_hash failed: {err}");
            return ExitCode::from(1);
        }
    };

    let pipe_name = match v2_program_pipe(&opts.program, &sid, SCAFFOLD_PIPE_IDX) {
        Ok(n) => n,
        Err(err) => {
            eprintln!("running-process-broker-v2: v2_program_pipe failed: {err}");
            return ExitCode::from(1);
        }
    };

    let socket_path = match resolve_socket_path(&pipe_name) {
        Ok(p) => p,
        Err(err) => {
            eprintln!("running-process-broker-v2: resolve_socket_path failed: {err}");
            return ExitCode::from(1);
        }
    };

    // Stale-file cleanup is Unix-only; on Windows the pipe namespace is
    // managed by the kernel and previous bindings vanish when the prior
    // process exited.
    #[cfg(unix)]
    {
        let path = std::path::Path::new(&socket_path);
        if let Some(parent) = path.parent() {
            if let Err(err) = std::fs::create_dir_all(parent) {
                eprintln!(
                    "running-process-broker-v2: create_dir_all({}) failed: {err}",
                    parent.display()
                );
                return ExitCode::from(1);
            }
        }
        let _ = std::fs::remove_file(&socket_path);
    }

    let name = match wrap_socket_name(&socket_path) {
        Ok(n) => n,
        Err(err) => {
            eprintln!("running-process-broker-v2: wrap_socket_name failed: {err}");
            return ExitCode::from(1);
        }
    };

    let listener = match ListenerOptions::new().name(name).create_sync() {
        Ok(l) => l,
        Err(err) => {
            // Single-instance enforcement: a `WouldBlock` / `AddrInUse`
            // bind failure means another `running-process-broker-v2
            // --program {program}` is already running on this user's
            // socket. Surface a directly-actionable message instead of
            // a raw OS error string.
            if is_already_bound_error(&err) {
                eprintln!(
                    "running-process-broker-v2: another broker is already \
                     bound at {socket_path} (program={}). Refusing to \
                     start to avoid double-bind. Stop the other broker \
                     first, or pass `--program <other-name>` to bind a \
                     distinct namespace.",
                    opts.program,
                );
                return ExitCode::from(75); // EX_TEMPFAIL — supervisor can retry after the other broker exits
            }
            eprintln!("running-process-broker-v2: bind failed at {socket_path}: {err}");
            return ExitCode::from(1);
        }
    };

    println!(
        "running-process-broker-v2 bound at {socket_path} (program={}, mode={})",
        opts.program,
        if opts.once { "once" } else { "loop" }
    );
    if let Err(err) = std::io::stdout().flush() {
        eprintln!("running-process-broker-v2: stdout flush failed: {err}");
    }

    let loader = Arc::new(ServiceDefinitionLoader::default_root());
    let inflight = Arc::new(AtomicUsize::new(0));

    // Optional HTTP aggregation surface (#483). Started after the control
    // socket is bound, so a broker that loses the single-instance race exits
    // before it can publish an endpoint another broker owns.
    let http = opts.http_port.and_then(|config| {
        match start_http_surface(config, &opts.program, &broker_v2_runtime_dir()) {
            Ok(started) => Some(started),
            Err(err) => {
                // Not fatal: the broker's job is the control socket, and the
                // HTTP page is an optional view onto it. Exiting here would
                // take out working brokering because a diagnostic page could
                // not bind.
                eprintln!("running-process-broker-v2: HTTP surface disabled: {err}");
                None
            }
        }
    });

    let exit_code = if opts.once {
        accept_one(
            &listener,
            Arc::clone(&loader),
            http.as_ref().map(|h| Arc::clone(&h.registry)),
        )
    } else {
        #[cfg(unix)]
        if let Err(err) = install_shutdown_signal_handlers() {
            eprintln!("running-process-broker-v2: install shutdown signal handlers failed: {err}");
            return ExitCode::from(1);
        }

        #[cfg(windows)]
        if let Err(err) = install_shutdown_console_handler() {
            eprintln!("running-process-broker-v2: install console control handler failed: {err}");
            return ExitCode::from(1);
        }

        #[cfg(unix)]
        let shutdown = &SIGNAL_SHUTDOWN_REQUESTED;
        #[cfg(windows)]
        let shutdown = &CONSOLE_SHUTDOWN_REQUESTED;
        // Kept so the loop still compiles on a target that is neither, where
        // there is no shutdown source to observe.
        #[cfg(not(any(unix, windows)))]
        let shutdown = &AtomicBool::new(false);

        accept_loop(
            &listener,
            Arc::clone(&loader),
            Arc::clone(&inflight),
            shutdown,
            http.as_ref().map(|h| Arc::clone(&h.registry)),
        )
    };

    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(&socket_path);
    }

    // Retract the published endpoint before exiting. A stale file would send
    // every reader to a port this process no longer listens on — worse than
    // no file at all, which readers already treat as "not running".
    if let Some(started) = &http {
        if let Err(err) =
            broker_http_discovery::unpublish_http_port(&broker_v2_runtime_dir(), &started.program)
        {
            eprintln!("running-process-broker-v2: could not unpublish HTTP endpoint: {err}");
        }
    }

    if let Err(err) = flush_coverage_profile() {
        eprintln!("running-process-broker-v2: coverage profile flush failed: {err}");
        return ExitCode::from(1);
    }

    exit_code
}

/// A running HTTP surface, kept so the endpoint can be retracted on exit.
struct HttpSurface {
    program: String,
    /// Shared with the accept path, which is what actually knows when a
    /// backend shows up. Without this the page renders correctly and always
    /// says "no backends registered yet".
    registry: Arc<HttpEndpointRegistry>,
}

/// Bind the HTTP aggregation surface, publish where it landed, and serve it.
///
/// # Publishing happens after binding, never before
///
/// The bound port is not always the requested one — `StaticOrFallback` falls
/// back to an OS-allocated port, and `Dynamic` never had a number to begin
/// with. Publishing the *requested* port would advertise an endpoint nobody
/// is listening on, and the reader has no way to tell that from a live one.
/// So the address published is the one taken from the bound listener.
/// `runtime_dir` is a parameter rather than derived inside so a test can
/// point it at a temp directory. Derived internally, the only way to exercise
/// this function would be to write into the real per-user runtime directory,
/// which collides with any broker actually running on the machine.
fn start_http_surface(
    config: BrokerHttpPort,
    program: &str,
    runtime_dir: &std::path::Path,
) -> Result<HttpSurface, String> {
    let registry = Arc::new(HttpEndpointRegistry::new());
    let server =
        BrokerHttpServer::bind(config, Arc::clone(&registry)).map_err(|e| e.to_string())?;
    let local = server.local_addr();

    let path =
        broker_http_discovery::publish_http_port(runtime_dir, program, local.ip(), local.port())
            .map_err(|e| format!("publishing {local} to {}: {e}", runtime_dir.display()))?;

    println!(
        "running-process-broker-v2 http at http://{local} (published to {})",
        path.display()
    );
    if let Err(err) = std::io::stdout().flush() {
        eprintln!("running-process-broker-v2: stdout flush failed: {err}");
    }

    // Detached: the surface is read-only and stateless, so there is nothing
    // to drain at shutdown. The thread ends with the process, and the
    // published file — the part that outlives it — is retracted by `main`.
    thread::Builder::new()
        .name("rpb-v2-http".to_string())
        .spawn(move || loop {
            if let Err(err) = server.serve_once() {
                // One failed accept says nothing about the next. Logging and
                // continuing keeps a transient error from silently taking the
                // page down for the life of the broker.
                eprintln!("running-process-broker-v2: http accept failed: {err}");
            }
        })
        .map_err(|e| format!("spawning the http thread: {e}"))?;

    Ok(HttpSurface {
        program: program.to_owned(),
        registry,
    })
}

/// Persistent accept loop. Spawns one handler thread per accepted
/// connection, bounded by `MAX_INFLIGHT_HANDLERS`. A nonblocking listener
/// makes the shutdown flag observable within [`ACCEPT_POLL_INTERVAL`].
fn accept_loop(
    listener: &interprocess::local_socket::Listener,
    loader: Arc<ServiceDefinitionLoader>,
    inflight: Arc<AtomicUsize>,
    shutdown: &AtomicBool,
    http: Option<Arc<HttpEndpointRegistry>>,
) -> ExitCode {
    if let Err(err) = listener.set_nonblocking(ListenerNonblockingMode::Accept) {
        eprintln!("running-process-broker-v2: set listener nonblocking failed: {err}");
        return ExitCode::from(1);
    }

    let max_inflight = max_inflight_handlers();
    let mut handlers = Vec::new();
    loop {
        reap_finished_handlers(&mut handlers);
        match poll_accept_until_shutdown(shutdown, || listener.accept()) {
            Ok(Some(stream)) => {
                // Backpressure: refuse to spawn if we're already at the cap.
                // The peer's blocking read on the Hello-reply socket will
                // see EOF when this branch closes the stream.
                let n = inflight.fetch_add(1, Ordering::SeqCst);
                if n >= max_inflight {
                    inflight.fetch_sub(1, Ordering::SeqCst);
                    eprintln!(
                        "running-process-broker-v2: at MAX_INFLIGHT_HANDLERS ({max_inflight}); dropping connection",
                    );
                    drop(stream);
                    continue;
                }
                let loader = Arc::clone(&loader);
                let inflight_handler = Arc::clone(&inflight);
                let http_handler = http.clone();
                let spawn_result = thread::Builder::new()
                    .name("rpb-v2-handler".to_string())
                    .spawn(move || {
                        let _inflight_guard = InflightGuard(inflight_handler);
                        let mut s = stream;
                        let result = handle_hello_with_deadline(&mut s, &loader);
                        match result {
                            Ok(svc) => {
                                // A negotiated Hello is the first moment a
                                // backend id exists, so it is where the
                                // aggregation page learns the backend is
                                // there. The port arrives separately; until
                                // then it renders as `(starting...)`.
                                if let Some(reg) = &http_handler {
                                    reg.track(svc.clone());
                                }
                                println!(
                                    "running-process-broker-v2 Hello service={svc:?} negotiated",
                                )
                            }
                            Err(err) => {
                                eprintln!("running-process-broker-v2 Hello handler failed: {err}")
                            }
                        }
                    });
                match spawn_result {
                    Ok(handler) => handlers.push(handler),
                    Err(err) => {
                        eprintln!(
                            "running-process-broker-v2: thread spawn failed: {err}; \
                             dropping connection"
                        );
                        // Decrement here since the spawned thread never ran.
                        inflight.fetch_sub(1, Ordering::SeqCst);
                    }
                }
            }
            Ok(None) => {
                println!("running-process-broker-v2: shutdown requested; draining handlers");
                drain_handlers(&mut handlers, HANDLER_DRAIN_TIMEOUT);
                return ExitCode::SUCCESS;
            }
            Err(err) => {
                // accept() errors are typically fatal (listener died);
                // exit so a supervisor can restart us.
                eprintln!("running-process-broker-v2: accept failed: {err}");
                return ExitCode::from(1);
            }
        }
    }
}

fn poll_accept_until_shutdown<T>(
    shutdown: &AtomicBool,
    mut accept: impl FnMut() -> std::io::Result<T>,
) -> std::io::Result<Option<T>> {
    while !shutdown.load(Ordering::Relaxed) {
        match accept() {
            Ok(value) => return Ok(Some(value)),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(None)
}

fn reap_finished_handlers(handlers: &mut Vec<thread::JoinHandle<()>>) {
    let mut index = 0;
    while index < handlers.len() {
        if handlers[index].is_finished() {
            let handler = handlers.swap_remove(index);
            if handler.join().is_err() {
                eprintln!("running-process-broker-v2: handler thread panicked");
            }
        } else {
            index += 1;
        }
    }
}

fn drain_handlers(handlers: &mut Vec<thread::JoinHandle<()>>, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !handlers.is_empty() && Instant::now() < deadline {
        reap_finished_handlers(handlers);
        if !handlers.is_empty() {
            thread::sleep(ACCEPT_POLL_INTERVAL);
        }
    }
    reap_finished_handlers(handlers);
    if !handlers.is_empty() {
        eprintln!(
            "running-process-broker-v2: handler drain timed out after {timeout:?}; \
             detaching {} handler(s)",
            handlers.len()
        );
    }
}

/// One-shot accept (replaces the prior scaffold behavior; used by
/// `--once` for tests + by the slice-3c integration test).
fn accept_one(
    listener: &interprocess::local_socket::Listener,
    loader: Arc<ServiceDefinitionLoader>,
    http: Option<Arc<HttpEndpointRegistry>>,
) -> ExitCode {
    match listener.accept() {
        Ok(mut stream) => {
            println!("running-process-broker-v2 peer connected (--once)");
            match handle_hello_with_deadline(&mut stream, &loader) {
                Ok(svc) => {
                    if let Some(reg) = &http {
                        reg.track(svc.clone());
                    }
                    println!(
                        "running-process-broker-v2 Hello for service {svc:?} negotiated; exiting"
                    );
                    ExitCode::SUCCESS
                }
                Err(err) => {
                    eprintln!("running-process-broker-v2: Hello handler failed: {err}");
                    ExitCode::from(1)
                }
            }
        }
        Err(err) => {
            eprintln!("running-process-broker-v2: accept failed: {err}");
            ExitCode::from(1)
        }
    }
}

fn handle_hello_with_deadline(
    stream: &mut interprocess::local_socket::Stream,
    loader: &ServiceDefinitionLoader,
) -> Result<String, String> {
    stream
        .set_nonblocking(true)
        .map_err(|error| format!("set Hello stream nonblocking: {error}"))?;
    let read_result = {
        let mut deadline_stream = DeadlineStream::new(stream, hello_read_deadline());
        read_frame(&mut deadline_stream).map_err(|error| format!("read Hello frame: {error}"))
    };
    let restore_result = stream
        .set_nonblocking(false)
        .map_err(|error| format!("restore Hello stream blocking mode: {error}"));
    let bytes = read_result?;
    restore_result?;
    handle_hello_bytes(stream, loader, bytes)
}

/// Read a `Hello` frame, look up the registered service, and send
/// back either `Negotiated` (service found + version policy OK) or
/// `Refused` (unknown service or policy block).
///
/// Returns the service name on Negotiated, or the human-readable
/// refusal reason on Refused. Wire errors propagate as `Err`.
fn handle_hello_bytes<S: std::io::Write>(
    stream: &mut S,
    loader: &ServiceDefinitionLoader,
    bytes: Vec<u8>,
) -> Result<String, String> {
    let hello = Hello::decode(bytes.as_slice()).map_err(|e| format!("decode Hello: {e}"))?;

    let backend_pipe = resolve_backend_pipe(&hello.service_name);
    let reply = build_hello_reply(&hello, loader, &backend_pipe);

    let mut body = Vec::with_capacity(reply.encoded_len());
    reply
        .encode(&mut body)
        .map_err(|e| format!("encode HelloReply: {e}"))?;
    write_frame(stream, &body).map_err(|e| format!("write HelloReply frame: {e}"))?;

    match reply.result {
        Some(hello_reply::Result::Negotiated(_)) => Ok(hello.service_name),
        Some(hello_reply::Result::Refused(r)) => Err(format!("refused: {}", r.reason)),
        None => Err("HelloReply missing result oneof".to_string()),
    }
}

/// Pure decision function — takes a Hello + a loader and returns the
/// HelloReply we should send. Split out from `handle_hello` so the
/// policy logic is unit-testable without standing up a real listener.
/// Where the daemon backing `service` can be reached, or empty if none has
/// published.
///
/// Reads the identity file the daemon writes when started with `--service`
/// (see running-process#532). The path comes from `daemon_identity_path`, the
/// same function the daemon publishes through, so the two cannot drift.
///
/// Absent is a normal state, not an error: a service can be registered and
/// version-compatible while its daemon has not started yet, or was launched
/// without `--service`. Callers get an empty pipe and the pre-#532 behaviour.
fn resolve_backend_pipe(service: &str) -> String {
    use running_process::broker::backend_sdk::read_daemon_identity_file;
    use running_process::broker::lifecycle::names_v2::daemon_identity_path;

    read_daemon_identity_file(&daemon_identity_path(service))
        .map(|daemon| daemon.ipc_endpoint.path)
        .unwrap_or_default()
}

fn build_hello_reply(
    hello: &Hello,
    loader: &ServiceDefinitionLoader,
    backend_pipe: &str,
) -> HelloReply {
    // 0. The client's protocol range must include the one we speak.
    //
    // Checked before the service lookup because it is cheaper and because it
    // is about the conversation itself: there is no point resolving a service
    // for a peer we cannot talk to. Without this the broker replied
    // `Negotiated { negotiated_protocol: 1 }` to a client that had just said
    // it speaks only 9999..=10000 — agreeing to a version the client had
    // explicitly ruled out, which it then cannot parse. v1 has always
    // rejected this (`hello_handler::validate_hello_shape`); v2 did not.
    if hello.client_min_protocol > ENVELOPE_VERSION as u32
        || hello.client_max_protocol < ENVELOPE_VERSION as u32
    {
        return refused_reply(
            hello,
            ErrorCode::ErrorVersionUnsupported,
            "client protocol range does not include the version this broker speaks",
            0,
        );
    }

    // 1. Look up the service. Unknown service → ErrorServiceUnknown.
    let definition = match loader.load(&hello.service_name) {
        Ok(d) => d,
        Err(ServiceDefinitionError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
            return refused_reply(
                hello,
                ErrorCode::ErrorServiceUnknown,
                "service definition was not found",
                0,
            );
        }
        Err(ServiceDefinitionError::InvalidName(_)) => {
            return refused_reply(
                hello,
                ErrorCode::ErrorServiceUnknown,
                "service name is invalid",
                0,
            );
        }
        Err(other) => {
            return refused_reply(
                hello,
                ErrorCode::ErrorServiceUnknown,
                format!("service definition could not be loaded: {other}"),
                0,
            );
        }
    };

    // 2. Version policy. min_version + version_allow_list per slice 22.
    if !definition.min_version.is_empty()
        && hello.wanted_version.as_str() < definition.min_version.as_str()
    {
        // Lexicographic for now (matches v1's pre-semver behaviour).
        // Real semver parsing is a follow-up; the contract is the
        // refusal reason + code, both already correct here.
        return refused_reply(
            hello,
            ErrorCode::ErrorVersionBlocked,
            format!(
                "wanted_version {:?} is below min_version {:?}",
                hello.wanted_version, definition.min_version
            ),
            0,
        );
    }
    if !definition.version_allow_list.is_empty()
        && !definition
            .version_allow_list
            .iter()
            .any(|v| v == &hello.wanted_version)
    {
        return refused_reply(
            hello,
            ErrorCode::ErrorVersionBlocked,
            format!(
                "wanted_version {:?} is not in version_allow_list",
                hello.wanted_version
            ),
            0,
        );
    }

    // 3. Happy path. `backend_pipe` is resolved by the caller — reading a
    //    daemon's published identity is I/O, and this stays a pure decision
    //    function so the policy above is testable without a filesystem.
    //
    //    An unresolved pipe is empty rather than a refusal: a service can be
    //    registered and version-compatible while its daemon has not started
    //    or was launched without `--service`. That is the pre-#532 behaviour,
    //    and refusing would turn a startup ordering detail into a hard error.
    //
    // `daemon_version` reports the **broker binary's own version**, not
    // `definition.min_version`. Min-version is a per-service floor
    // expressed *by* the service definition; daemon_version is the
    // running broker's actual version — they are unrelated. Using
    // min_version here regressed the original behavior (see PR #533's
    // diff) and yields an empty string for any servicedef that
    // doesn't explicitly opt in to a floor, which violates the test's
    // and the proto's "non-empty" expectation.
    HelloReply {
        result: Some(hello_reply::Result::Negotiated(Negotiated {
            negotiated_protocol: ENVELOPE_VERSION as u32,
            daemon_version: env!("CARGO_PKG_VERSION").into(),
            backend_pipe: backend_pipe.to_string(),
            warnings: Vec::new(),
            server_capabilities: 0,
            keepalive_interval_secs: 0,
            handle_passed_token: Vec::new(),
            connection_id: hello.connection_id,
        })),
    }
}

fn refused_reply(
    hello: &Hello,
    code: ErrorCode,
    reason: impl Into<String>,
    retry_after_ms: u64,
) -> HelloReply {
    HelloReply {
        result: Some(hello_reply::Result::Refused(Refused {
            reason: reason.into(),
            daemon_min_protocol: ENVELOPE_VERSION as u32,
            daemon_max_protocol: ENVELOPE_VERSION as u32,
            code: code as i32,
            details: std::collections::HashMap::new(),
            retry_after_ms,
        })),
    }
    .with_connection_id(hello.connection_id)
}

trait HelloReplyExt {
    fn with_connection_id(self, id: u64) -> Self;
}

impl HelloReplyExt for HelloReply {
    fn with_connection_id(mut self, id: u64) -> Self {
        if let Some(hello_reply::Result::Refused(_)) = &self.result {
            // Refused has no connection_id; nothing to thread.
        } else if let Some(hello_reply::Result::Negotiated(ref mut n)) = self.result {
            n.connection_id = id;
        }
        self
    }
}

/// Wrap a bare pipe name into the platform's local-socket path.
fn resolve_socket_path(bare_name: &str) -> Result<String, String> {
    #[cfg(windows)]
    {
        Ok(format!(r"\\.\pipe\{bare_name}"))
    }
    #[cfg(unix)]
    {
        let dir = unix_socket_dir();
        let leaf = if cfg!(target_os = "macos") {
            // macOS sun_path is 104 bytes; hash the bare name to fit.
            let mut hash = blake3::Hasher::new();
            hash.update(bare_name.as_bytes());
            let bytes = hash.finalize();
            let mut hex = String::with_capacity(16);
            for b in bytes.as_bytes().iter().take(8) {
                use std::fmt::Write as _;
                let _ = write!(hex, "{b:02x}");
            }
            format!("{hex}.sock")
        } else {
            format!("{bare_name}.sock")
        };
        Ok(dir.join(leaf).to_string_lossy().into_owned())
    }
}

#[cfg(unix)]
fn unix_socket_dir() -> std::path::PathBuf {
    use std::path::PathBuf;
    #[cfg(target_os = "macos")]
    {
        let uid = unsafe { libc::getuid() };
        let tmp = env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        tmp.join(format!(".rp-{uid}-broker-v2"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        if let Some(d) = env::var_os("XDG_RUNTIME_DIR") {
            PathBuf::from(d).join("running-process").join("broker-v2")
        } else {
            let uid = unsafe { libc::getuid() };
            PathBuf::from(format!("/tmp/running-process-{uid}/broker-v2"))
        }
    }
}

/// Classify a [`ListenerOptions::create_sync`] error as
/// "another broker is already bound" vs any other bind failure.
///
/// `AddrInUse` / `WouldBlock` are the canonical "another listener
/// already owns this name" signals on Unix-style transports.
/// **Windows named-pipe bind reports the same condition as
/// `PermissionDenied`** (ERROR_ACCESS_DENIED, raw os error 5)
/// because the existing pipe instance's ACL blocks the second bind.
/// Treat that case as already-bound too — a "true" permission
/// problem on the v2 broker socket path is extremely rare in
/// production (the path lives under XDG_RUNTIME_DIR / TMPDIR which
/// is always writable by the current user).
fn is_already_bound_error(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::AddrInUse
            | std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::PermissionDenied,
    )
}

fn wrap_socket_name(socket_path: &str) -> Result<interprocess::local_socket::Name<'_>, String> {
    use interprocess::local_socket::prelude::*;
    #[cfg(windows)]
    {
        use interprocess::local_socket::GenericNamespaced;
        let bare = socket_path
            .strip_prefix(r"\\.\pipe\")
            .unwrap_or(socket_path);
        bare.to_ns_name::<GenericNamespaced>()
            .map_err(|e| format!("to_ns_name: {e}"))
    }
    #[cfg(unix)]
    {
        use interprocess::local_socket::GenericFilePath;
        socket_path
            .to_fs_name::<GenericFilePath>()
            .map_err(|e| format!("to_fs_name: {e}"))
    }
}

#[cfg(test)]
#[path = "running-process-broker-v2/tests.rs"]
mod tests;
