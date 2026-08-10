//! Production-rollout latency evidence for the handle-passing handoff
//! (#387).
//!
//! Unlike the in-process `handoff_serve_latency` benchmark (which runs
//! both the serve loop and the client inside one test binary), this
//! driver deploys the production topology as REAL separate OS processes:
//!
//! - a **server process** (this example re-invoked in `server` role) that
//!   runs the production [`serve_registered_backend`] accept loop — the
//!   same function `running-process-broker-v1 --serve` invokes — plus the
//!   backend side of the deployment: the startup endpoint-identity probe
//!   and either the production offer/ACK handoff wire exchange
//!   (`backend_lib::wire`) or the `backend_pipe` reconnect listener;
//! - a **client process** (this example in `driver` role) using the
//!   public `connect_to_backend` opt-in
//!   (`ConnectBackendRequest::adopt_handed_off_connection`).
//!
//! Broker and backend are co-located in the server process by
//! architectural necessity, not convenience: `serve_registered_backend`
//! verifies the startup endpoint probe against the serving process's OWN
//! identity (pid / exe_path / exe_sha256), so the registered-backend
//! serve mode only supports a backend that embeds the serve loop. A
//! standalone broker binary fronting a separate backend process would
//! fail startup verification with `IdentityMismatch { field: "pid" }`.
//!
//! Two deployments are measured with the `handoff_latency_e2e`
//! methodology (`collect_latency_samples`: 5 warmup + 50 measured
//! iterations, monotonic `Instant` timing, nearest-rank P50/P99 via
//! `summarize_latency_samples`):
//!
//! - **handoff**: server configured `with_handoff_endpoint`; every client
//!   connect performs the full Hello, the server executes the platform
//!   transfer (`DuplicateHandle` on Windows, `sendmsg(SCM_RIGHTS)` on
//!   Unix), the backend side ACKs, the broker relays handoff-ready, and
//!   the client adopts the connection that carried Hello
//!   (`BackendConnectionRoute::HandlePassed`).
//! - **reconnect**: same server without a handoff endpoint (the
//!   production default); the client reconnects through the negotiated
//!   `backend_pipe` (`BackendConnectionRoute::BrokerNegotiated`).
//!
//! Every timed sample crosses a real process boundary and ends with one
//! probe/reply byte round trip on the resulting backend connection, so
//! each sample proves the route serves traffic. Measured numbers are
//! recorded in `docs/v1-handoff-optimization.md`.
//!
//! Usage:
//!
//! ```text
//! handoff_rollout_evidence driver
//! ```
//!
//! This is a dev-only evidence harness, not shipped tooling; the
//! `server` role is an internal re-invocation detail.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

use interprocess::local_socket::prelude::*;
use interprocess::local_socket::ListenerOptions;
use prost::Message;
use running_process::broker::backend_handle::DaemonProcess;
use running_process::broker::backend_lib::wire::{read_handoff_offer, respond_to_handoff_offer};
use running_process::broker::backend_lifecycle::probe::handle_endpoint_probe;
use running_process::broker::client::{
    connect_to_backend, BackendConnection, BackendConnectionRoute, ConnectBackendRequest,
};
use running_process::broker::protocol::{
    BrokerIsolation, Endpoint, HandoffOffer, ServiceDefinition,
};
use running_process::broker::server::handoff::{
    collect_latency_samples, summarize_latency_samples, HandoffLatencySummary,
};
use running_process::broker::server::{
    ensure_service_definition_dir, local_socket_name, serve_registered_backend,
    service_definition_path, BrokerInstanceKey, BrokerServeConfig, HandoffToken, HandoffTokenStore,
    HANDOFF_TOKEN_BYTES,
};

const SERVICE_NAME: &str = "zccache";
const SERVICE_VERSION: &str = "1.11.20";
const CLIENT_PROBE: u8 = 0xC3;
const BACKEND_REPLY: u8 = 0x5A;
const WARMUP_ITERATIONS: usize = 5;
const MEASURED_ITERATIONS: usize = 50;
const TOTAL_CONNECTIONS: usize = WARMUP_ITERATIONS + MEASURED_ITERATIONS;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("driver") => run_driver(),
        Some("server") => run_server(&args[1..]),
        _ => {
            eprintln!("usage: handoff_rollout_evidence driver");
            std::process::exit(2);
        }
    }
}

// ---------------------------------------------------------------------------
// Driver role: deploy the server process, measure both routes as a real
// out-of-process client.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Handoff,
    Reconnect,
}

fn run_driver() {
    let handoff = measure_deployment(Mode::Handoff);
    let reconnect = measure_deployment(Mode::Reconnect);
    println!(
        "rollout-evidence[{os}]: handoff p50={}us p99={}us (n={}) \
         reconnect p50={}us p99={}us (n={})",
        handoff.p50.as_micros(),
        handoff.p99.as_micros(),
        handoff.sample_count,
        reconnect.p50.as_micros(),
        reconnect.p99.as_micros(),
        reconnect.sample_count,
        os = std::env::consts::OS,
    );
}

/// Deploy one real server process and measure the client-visible connect
/// latency of the requested route from this (separate) client process.
fn measure_deployment(mode: Mode) -> HandoffLatencySummary {
    let tmp = tempfile::tempdir().expect("temp dir for deployment");
    let services = tmp.path().join("services");
    write_service_definition(&services);

    let label = match mode {
        Mode::Handoff => "387ho",
        Mode::Reconnect => "387rc",
    };
    let broker_socket = unique_socket_name(&format!("{label}-brk"));
    let backend_endpoint = unique_socket_name(&format!("{label}-be"));
    let handoff_endpoint = unique_socket_name(&format!("{label}-hoff"));
    let ready_file = tmp.path().join("server-ready");

    let self_exe = std::env::current_exe().expect("driver binary path");
    let mut server_cmd = Command::new(&self_exe);
    server_cmd.arg("server");
    match mode {
        Mode::Handoff => server_cmd
            .arg("handoff")
            .arg(&broker_socket)
            .arg(&backend_endpoint)
            .arg(&handoff_endpoint),
        Mode::Reconnect => server_cmd
            .arg("reconnect")
            .arg(&broker_socket)
            .arg(&backend_endpoint),
    };
    server_cmd
        .arg(&ready_file)
        .arg(&services)
        .env("RUNNING_PROCESS_DAEMON_SCOPE", "dev");
    let mut server = ChildGuard::spawn(server_cmd, "server");
    wait_for_file(&ready_file, Duration::from_secs(15));
    if mode == Mode::Reconnect {
        // Wait until the server has reclaimed the backend endpoint from
        // the startup-probe listener; otherwise an early client reconnect
        // would dial an unbound endpoint and burn a broker serve slot.
        wait_for_file(&serving_marker(&ready_file), Duration::from_secs(15));
    }

    let samples = collect_latency_samples(WARMUP_ITERATIONS, MEASURED_ITERATIONS, || {
        // Timed region: the full client-visible connect through the real
        // server process (Hello, then platform handoff + handoff-ready
        // relay + adoption, or backend_pipe reconnect) plus one
        // probe/reply round trip proving the route serves traffic.
        let started = Instant::now();
        let mut connection = connect_with_retry(&broker_socket, mode == Mode::Handoff);
        let reply = probe_roundtrip(&mut connection);
        let elapsed = started.elapsed();
        let expected_route = match mode {
            Mode::Handoff => BackendConnectionRoute::HandlePassed,
            Mode::Reconnect => BackendConnectionRoute::BrokerNegotiated,
        };
        assert_eq!(connection.route, expected_route, "unexpected route");
        assert_eq!(reply, BACKEND_REPLY, "backend must answer the probe");
        drop(connection);
        elapsed
    });

    server.wait_success();
    summarize(&samples, label)
}

/// One probe byte out, one reply byte back, on the backend connection.
fn probe_roundtrip(connection: &mut BackendConnection) -> u8 {
    connection
        .stream
        .write_all(&[CLIENT_PROBE])
        .expect("probe write");
    let mut reply = [0_u8; 1];
    connection
        .stream
        .read_exact(&mut reply)
        .expect("reply read");
    reply[0]
}

/// Client connect through the server process, retrying only while the
/// broker socket is not yet bound. Each successful dial performs one full
/// Hello negotiation against the real serve loop.
fn connect_with_retry(broker_endpoint: &str, adopt: bool) -> BackendConnection {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let mut request = ConnectBackendRequest::new(
            broker_endpoint,
            SERVICE_NAME,
            SERVICE_VERSION,
            SERVICE_VERSION,
        );
        request.adopt_handed_off_connection = adopt;
        request.handoff_ready_timeout = Duration::from_secs(10);
        match connect_to_backend(request) {
            Ok(connection) => return connection,
            Err(err) => {
                assert!(
                    Instant::now() < deadline,
                    "timed out connecting through broker {broker_endpoint}: {err}"
                );
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

fn summarize(samples: &[Duration], label: &str) -> HandoffLatencySummary {
    assert_eq!(
        samples.len(),
        MEASURED_ITERATIONS,
        "{label} must collect every measured iteration"
    );
    assert!(
        samples.iter().all(|sample| *sample > Duration::ZERO),
        "{label} samples must be non-zero monotonic-clock durations"
    );
    let summary = summarize_latency_samples(samples).expect("non-empty sample set");
    assert!(
        summary.p50 <= summary.p99,
        "{label} P50 must not exceed P99"
    );
    summary
}

fn write_service_definition(root: &Path) {
    let exe = std::env::current_exe().expect("server binary path");
    let dir = exe.parent().expect("server binary dir").to_path_buf();
    let definition = ServiceDefinition {
        service_name: SERVICE_NAME.into(),
        binary_path: exe.to_string_lossy().into_owned(),
        isolation: BrokerIsolation::SharedBroker as i32,
        explicit_instance: String::new(),
        per_version_binary_dir: dir.to_string_lossy().into_owned(),
        min_version: "1.10.0".into(),
        version_allow_list: vec![SERVICE_VERSION.into()],
        labels: Default::default(),
    };
    ensure_service_definition_dir(root).expect("service definition dir");
    let path = service_definition_path(root, SERVICE_NAME).expect("service definition path");
    std::fs::write(path, definition.encode_to_vec()).expect("write service definition");
}

fn serving_marker(ready_file: &Path) -> PathBuf {
    let mut marker = ready_file.as_os_str().to_owned();
    marker.push(".serving");
    PathBuf::from(marker)
}

fn wait_for_file(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for readiness marker {path:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

/// Kill-on-drop child wrapper so a panicking benchmark never leaks the
/// server process.
struct ChildGuard {
    child: Option<Child>,
    label: &'static str,
}

impl ChildGuard {
    fn spawn(mut command: Command, label: &'static str) -> Self {
        let child = command
            .spawn()
            .unwrap_or_else(|err| panic!("failed to spawn {label} process: {err}"));
        Self {
            child: Some(child),
            label,
        }
    }

    fn wait_success(&mut self) {
        let mut child = self.child.take().expect("child already waited");
        let status = child
            .wait()
            .unwrap_or_else(|err| panic!("failed to wait for {} process: {err}", self.label));
        assert!(
            status.success(),
            "{} process exited with {status}",
            self.label
        );
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

// ---------------------------------------------------------------------------
// Server role: the production serve loop plus the backend side, in one
// real OS process (the topology `serve_registered_backend` supports).
// ---------------------------------------------------------------------------

fn run_server(args: &[String]) {
    match args.first().map(String::as_str) {
        Some("handoff") => {
            let [broker_socket, backend_endpoint, handoff_endpoint, ready_file, services] =
                &args[1..]
            else {
                panic!(
                    "server handoff requires <broker-socket> <backend-endpoint> \
                     <handoff-endpoint> <ready-file> <service-def-dir>"
                );
            };
            run_handoff_server(
                broker_socket,
                backend_endpoint,
                handoff_endpoint,
                Path::new(ready_file),
                Path::new(services),
            );
        }
        Some("reconnect") => {
            let [broker_socket, backend_endpoint, ready_file, services] = &args[1..] else {
                panic!(
                    "server reconnect requires <broker-socket> <backend-endpoint> \
                     <ready-file> <service-def-dir>"
                );
            };
            run_reconnect_server(
                broker_socket,
                backend_endpoint,
                Path::new(ready_file),
                Path::new(services),
            );
        }
        other => panic!("unknown server mode {other:?}"),
    }
}

fn serve_config(broker_socket: &str, backend_endpoint: &str, services: &Path) -> BrokerServeConfig {
    BrokerServeConfig::new(
        broker_socket,
        SERVICE_NAME,
        SERVICE_VERSION,
        backend_endpoint,
        TOTAL_CONNECTIONS,
    )
    .expect("serve config")
    .with_service_definition_dir(services)
}

/// Handoff-mode server: answer the startup endpoint probe, run the
/// production serve loop with the handoff endpoint configured, and serve
/// one offer/ACK handoff exchange (plus adopted-connection probe/reply)
/// per client connection.
fn run_handoff_server(
    broker_socket: &str,
    backend_endpoint: &str,
    handoff_endpoint: &str,
    ready_file: &Path,
    services: &Path,
) {
    // Bind both backend listeners before the serve loop starts: the
    // startup probe fires during serve startup and the handoff endpoint
    // is dialed once per negotiated connection.
    let handoff_listener = bind_socket(handoff_endpoint).expect("bind handoff endpoint");
    let probe_listener = bind_socket(backend_endpoint).expect("bind backend endpoint");
    let probe_thread = spawn_probe_answer(probe_listener, backend_endpoint.to_owned());
    let handoff_endpoint_owned = handoff_endpoint.to_owned();
    let handoff_thread = thread::spawn(move || -> io::Result<()> {
        for _ in 0..TOTAL_CONNECTIONS {
            let mut stream = handoff_listener.accept()?;
            serve_one_handoff(&mut stream)?;
        }
        cleanup_socket(&handoff_endpoint_owned);
        Ok(())
    });
    std::fs::write(ready_file, b"ready").expect("write ready file");

    let config = serve_config(broker_socket, backend_endpoint, services)
        .with_handoff_endpoint(handoff_endpoint);
    serve_registered_backend(config).expect("serve loop");
    probe_thread.join().expect("probe thread").expect("probe");
    handoff_thread
        .join()
        .expect("handoff thread")
        .expect("handoff exchanges");
}

/// Reconnect-mode server: answer the startup probe, run the production
/// serve loop with handoff disabled (the default), and serve one
/// probe/reply exchange per reconnecting client on the backend endpoint.
fn run_reconnect_server(
    broker_socket: &str,
    backend_endpoint: &str,
    ready_file: &Path,
    services: &Path,
) {
    let probe_listener = bind_socket(backend_endpoint).expect("bind backend endpoint");
    let endpoint = backend_endpoint.to_owned();
    let marker = serving_marker(ready_file);
    let backend_thread = thread::spawn(move || -> io::Result<()> {
        answer_probe_once(probe_listener, &endpoint);
        // Rebind with retry while the just-dropped probe listener may
        // still hold the pipe name.
        let deadline = Instant::now() + Duration::from_secs(10);
        let listener = loop {
            match bind_socket(&endpoint) {
                Ok(listener) => break listener,
                Err(err) => {
                    assert!(
                        Instant::now() < deadline,
                        "could not reclaim backend endpoint {endpoint}: {err}"
                    );
                    thread::sleep(Duration::from_millis(10));
                }
            }
        };
        std::fs::write(marker, b"serving")?;
        for _ in 0..TOTAL_CONNECTIONS {
            let mut stream = listener.accept()?;
            let mut probe = [0_u8; 1];
            stream.read_exact(&mut probe)?;
            if probe != [CLIENT_PROBE] {
                return Err(io::Error::other("unexpected probe byte"));
            }
            stream.write_all(&[BACKEND_REPLY])?;
        }
        cleanup_socket(&endpoint);
        Ok(())
    });
    std::fs::write(ready_file, b"ready").expect("write ready file");

    serve_registered_backend(serve_config(broker_socket, backend_endpoint, services))
        .expect("serve loop");
    backend_thread
        .join()
        .expect("backend thread")
        .expect("reconnect exchanges");
}

/// Answer the serve loop's startup endpoint identity probe with this
/// process's real identity, then drop the listener.
fn spawn_probe_answer(
    listener: interprocess::local_socket::Listener,
    backend_endpoint: String,
) -> thread::JoinHandle<io::Result<()>> {
    thread::spawn(move || {
        answer_probe_once(listener, &backend_endpoint);
        Ok(())
    })
}

fn answer_probe_once(listener: interprocess::local_socket::Listener, backend_endpoint: &str) {
    let endpoint = Endpoint {
        namespace_id: BrokerInstanceKey::Shared.id(),
        path: backend_endpoint.into(),
    };
    let daemon = DaemonProcess::current_process(endpoint, Some(30)).expect("server identity");
    let mut stream = listener.accept().expect("accept startup probe");
    handle_endpoint_probe(&mut stream, &daemon).expect("answer startup probe");
    drop(stream);
    cleanup_socket(backend_endpoint);
}

/// Answer one offer through the production backend wire path, accepting
/// with the offered token seeded as pending.
fn respond_accept<S: Write>(stream: &mut S, offer: HandoffOffer) -> io::Result<()> {
    let now = Instant::now();
    let mut pending_tokens = HandoffTokenStore::new();
    let bytes = <[u8; HANDOFF_TOKEN_BYTES]>::try_from(offer.token.as_slice())
        .map_err(|_| io::Error::other("offered token is not 16 bytes"))?;
    let expected_token: HandoffToken = pending_tokens
        .issue_with_random128(now, || Ok(bytes))
        .map_err(io::Error::other)?;
    respond_to_handoff_offer(stream, &mut pending_tokens, expected_token, offer, now)
        .map_err(io::Error::other)?;
    Ok(())
}

#[cfg(windows)]
fn serve_one_handoff(stream: &mut interprocess::local_socket::Stream) -> io::Result<()> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};

    let offer = read_handoff_offer(stream).map_err(io::Error::other)?;
    let handle_value = offer.handle_value;
    respond_accept(stream, offer)?;
    // Adopt the handle the broker duplicated for the backend and prove it
    // serves the client's connection. The pipe was created overlapped by
    // the broker's listener, so the byte exchange uses explicit
    // OVERLAPPED I/O on the raw handle.
    let adopted = unsafe { OwnedHandle::from_raw_handle(handle_value as RawHandle) };
    let mut probe = [0_u8; 1];
    overlapped_transfer(adopted.as_raw_handle(), &mut probe, false)?;
    if probe != [CLIENT_PROBE] {
        return Err(io::Error::other(
            "unexpected probe byte on adopted connection",
        ));
    }
    overlapped_transfer(adopted.as_raw_handle(), &mut [BACKEND_REPLY], true)?;
    Ok(())
}

/// One blocking overlapped read (`write == false`) or write (`write ==
/// true`) on a duplicated overlapped pipe handle.
#[cfg(windows)]
fn overlapped_transfer(
    handle: std::os::windows::io::RawHandle,
    buffer: &mut [u8],
    write: bool,
) -> io::Result<()> {
    use winapi::shared::winerror::ERROR_IO_PENDING;
    use winapi::um::errhandlingapi::GetLastError;
    use winapi::um::fileapi::{ReadFile, WriteFile};
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::ioapiset::GetOverlappedResult;
    use winapi::um::minwinbase::OVERLAPPED;
    use winapi::um::synchapi::CreateEventW;

    unsafe {
        let event = CreateEventW(std::ptr::null_mut(), 1, 0, std::ptr::null());
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut overlapped: OVERLAPPED = std::mem::zeroed();
        overlapped.hEvent = event;
        let mut transferred = 0_u32;
        let immediate = if write {
            WriteFile(
                handle.cast(),
                buffer.as_ptr().cast(),
                buffer.len() as u32,
                &mut transferred,
                &mut overlapped,
            )
        } else {
            ReadFile(
                handle.cast(),
                buffer.as_mut_ptr().cast(),
                buffer.len() as u32,
                &mut transferred,
                &mut overlapped,
            )
        };
        let result = if immediate != 0 {
            Ok(())
        } else if GetLastError() == ERROR_IO_PENDING {
            if GetOverlappedResult(handle.cast(), &mut overlapped, &mut transferred, 1) != 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        } else {
            Err(io::Error::last_os_error())
        };
        CloseHandle(event);
        result?;
        if transferred as usize != buffer.len() {
            return Err(io::Error::other("short overlapped pipe transfer"));
        }
        Ok(())
    }
}

#[cfg(unix)]
fn serve_one_handoff(stream: &mut interprocess::local_socket::Stream) -> io::Result<()> {
    use std::os::fd::{AsFd, AsRawFd, FromRawFd};
    use std::os::unix::net::UnixStream;

    // The fd plus token ride SCM_RIGHTS on the same handoff connection
    // that then carries the offer frame.
    let socket_fd = match &*stream {
        interprocess::local_socket::Stream::UdSocket(socket) => socket.as_fd().as_raw_fd(),
    };
    let (received_fd, _token) = recv_fd_and_token(socket_fd)?;
    let offer = read_handoff_offer(stream).map_err(io::Error::other)?;
    respond_accept(stream, offer)?;
    let mut adopted = unsafe { UnixStream::from_raw_fd(received_fd) };
    let mut probe = [0_u8; 1];
    adopted.read_exact(&mut probe)?;
    if probe != [CLIENT_PROBE] {
        return Err(io::Error::other(
            "unexpected probe byte on adopted connection",
        ));
    }
    adopted.write_all(&[BACKEND_REPLY])
}

#[cfg(unix)]
fn recv_fd_and_token(
    socket_fd: std::os::fd::RawFd,
) -> io::Result<(std::os::fd::RawFd, [u8; HANDOFF_TOKEN_BYTES])> {
    let mut token = [0_u8; HANDOFF_TOKEN_BYTES];
    let mut iov = libc::iovec {
        iov_base: token.as_mut_ptr().cast(),
        iov_len: token.len(),
    };
    let mut control =
        vec![0_u8; unsafe { libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as _) as usize }];
    let mut message = unsafe { std::mem::zeroed::<libc::msghdr>() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control.len() as _;

    let received = unsafe { libc::recvmsg(socket_fd, &mut message, 0) };
    if received as usize != token.len() {
        return Err(io::Error::other("short SCM_RIGHTS handoff read"));
    }
    let header = unsafe { libc::CMSG_FIRSTHDR(&message) };
    if header.is_null() {
        return Err(io::Error::other("missing SCM_RIGHTS control message"));
    }
    unsafe {
        if (*header).cmsg_level != libc::SOL_SOCKET || (*header).cmsg_type != libc::SCM_RIGHTS {
            return Err(io::Error::other("unexpected handoff control message"));
        }
        Ok((*libc::CMSG_DATA(header).cast::<libc::c_int>(), token))
    }
}

// ---------------------------------------------------------------------------
// Socket plumbing (mirrors the broker test harness's socket_common).
// ---------------------------------------------------------------------------

/// Build a unique IPC endpoint name keyed by `label`. Short Unix leaf
/// names keep the whole path inside `sun_path` limits.
fn unique_socket_name(label: &str) -> String {
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();

    #[cfg(windows)]
    {
        format!("rpb-v1-{label}-{}-{suffix}", std::process::id())
    }

    #[cfg(unix)]
    {
        let short_suffix = suffix % 1_000_000_000;
        std::env::temp_dir()
            .join(format!(
                "rp-{label}-{}-{short_suffix}.sock",
                std::process::id()
            ))
            .to_string_lossy()
            .into_owned()
    }
}

/// Bind a fresh listener on `socket_name`, clearing any stale Unix socket
/// file first.
fn bind_socket(socket_name: &str) -> io::Result<interprocess::local_socket::Listener> {
    #[cfg(unix)]
    {
        let path = std::path::Path::new(socket_name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _ = std::fs::remove_file(path);
    }

    let name = local_socket_name(socket_name)?;
    ListenerOptions::new().name(name).create_sync()
}

fn cleanup_socket(socket_name: &str) {
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(socket_name);
    }

    #[cfg(windows)]
    let _ = socket_name;
}
