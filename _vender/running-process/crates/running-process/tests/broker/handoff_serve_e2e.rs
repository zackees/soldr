//! End-to-end handle-passing handoff through the production serve path
//! (#387).
//!
//! Runs the real `serve_registered_backend` accept loop with the opt-in
//! `handoff_endpoint` configured, a real endpoint-probe backend, and a
//! backend handoff listener speaking the production offer/ACK wire
//! protocol (`backend_lib::wire`). An opted-in `connect_to_backend`
//! client must end up with `BackendConnectionRoute::HandlePassed` and
//! serve traffic on the very socket that carried Hello; a backend that
//! rejects the offer must silently downgrade the client to the
//! `backend_pipe` reconnect (`BrokerNegotiated`) with no client-visible
//! error — the frozen correctness contract.

#![cfg(feature = "client")]

use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use interprocess::local_socket::traits::Listener as _;
use prost::Message;
use running_process::broker::backend_handle::DaemonProcess;
use running_process::broker::backend_lib::wire::{
    read_handoff_offer_with_deadline, respond_to_handoff_offer,
};
use running_process::broker::client::{
    connect_local_socket, connect_to_backend, BackendConnection, BackendConnectionRoute,
    ConnectBackendRequest,
};
use running_process::broker::protocol::{
    BrokerIsolation, Endpoint, HandoffOffer, ServiceDefinition,
};
use running_process::broker::server::{
    ensure_service_definition_dir, serve_registered_backend, service_definition_path,
    BrokerInstanceKey, BrokerServeConfig, BrokerServeError, HandoffToken, HandoffTokenStore,
    HANDOFF_TOKEN_BYTES,
};

use crate::backend_handle_common::spawn_endpoint_probe_once;
use crate::socket_common::{
    await_test_socket_ready, bind_ready_test_socket, bind_test_socket, cleanup_test_socket,
    unique_socket_name,
};

pub(crate) const CLIENT_PROBE: u8 = 0xC3;
pub(crate) const BACKEND_REPLY: u8 = 0x5A;

#[test]
fn serve_handoff_completes_and_client_adopts_connection() {
    let _watchdog = test_watchdog::install(
        Duration::from_secs(90),
        "serve_handoff_completes_and_client_adopts_connection appears to be hung",
        None,
    );
    let tmp = write_service_definition_dir();
    let socket_name = unique_socket_name("handoff-serve-ok");
    // No listener is ever re-bound on the backend endpoint after the
    // startup probe: a wrong fallback to reconnect would fail loudly.
    let backend_endpoint = unique_socket_name("handoff-serve-ok-be");
    let handoff_endpoint = unique_socket_name("handoff-serve-ok-ho");
    let backend_probe = spawn_configured_backend_probe(&backend_endpoint);
    let handoff_backend =
        spawn_backend_handoff_listener(handoff_endpoint.clone(), BackendBehavior::Accept);
    let config = serve_config(
        tmp.path().join("services").as_path(),
        socket_name.clone(),
        backend_endpoint.clone(),
        1,
    )
    .with_handoff_endpoint(handoff_endpoint);
    let (server, serve_errors) = spawn_serve(config);

    let mut connection = connect_backend_with_retry(
        &socket_name,
        Duration::from_secs(10),
        &server,
        &serve_errors,
    );

    assert_eq!(connection.route, BackendConnectionRoute::HandlePassed);
    assert_eq!(
        connection.endpoint, backend_endpoint,
        "adopted connections must still report backend_pipe for hello-skip caching"
    );
    assert!(connection.handoff_token().is_some());

    // Prove the SAME socket that carried Hello now serves backend traffic.
    connection.stream.write_all(&[CLIENT_PROBE]).unwrap();
    let mut reply = [0_u8; 1];
    connection.stream.read_exact(&mut reply).unwrap();
    assert_eq!(reply, [BACKEND_REPLY]);

    drop(connection.stream);
    server.join().unwrap().unwrap();
    backend_probe.join().unwrap().unwrap();
    handoff_backend.join().unwrap().unwrap();
}

#[test]
fn rejected_handoff_silently_downgrades_to_backend_pipe() {
    let _watchdog = test_watchdog::install(
        Duration::from_secs(90),
        "rejected_handoff_silently_downgrades_to_backend_pipe appears to be hung",
        None,
    );
    let tmp = write_service_definition_dir();
    let socket_name = unique_socket_name("handoff-serve-rej");
    let backend_endpoint = unique_socket_name("handoff-serve-rej-be");
    let handoff_endpoint = unique_socket_name("handoff-serve-rej-ho");
    let backend_probe = spawn_configured_backend_probe(&backend_endpoint);
    let handoff_backend =
        spawn_backend_handoff_listener(handoff_endpoint.clone(), BackendBehavior::Reject);
    // Spare serve slots: a connection attempt that fails mid-negotiation on
    // a slow runner still consumes one bounded-accept slot, and with a
    // single slot the broker would exit (unlinking its socket) while the
    // client keeps retrying against ENOENT for the full deadline. Leftover
    // slots are drained below so the bounded loop still terminates.
    let config = serve_config(
        tmp.path().join("services").as_path(),
        socket_name.clone(),
        backend_endpoint.clone(),
        REJECT_SERVE_CONNECTIONS,
    )
    .with_handoff_endpoint(handoff_endpoint);
    let (server, serve_errors) = spawn_serve(config);
    // The startup probe owns the backend endpoint until verification ends;
    // only then can the reconnect listener take its place.
    backend_probe.join().unwrap().unwrap();
    let reconnect_backend = ReconnectBackend::spawn(backend_endpoint.clone());

    // The ready wait is EOF-bounded on Unix (the rejecting backend closes
    // the transferred fd) but runs to the full timeout on Windows, where
    // the duplicated handle stays open in the backend process — so keep it
    // bounded, just not so tight a loaded runner can race it.
    let connection =
        connect_backend_with_retry(&socket_name, Duration::from_secs(2), &server, &serve_errors);

    assert_eq!(connection.route, BackendConnectionRoute::BrokerNegotiated);
    assert_eq!(connection.endpoint, backend_endpoint);
    drop(connection.stream);
    drain_serve_slots(&server, &socket_name);
    server.join().unwrap().unwrap();
    handoff_backend.join().unwrap().unwrap();
    reconnect_backend.stop().unwrap();
}

/// Serve slots for the reject test: one for the asserted downgrade
/// connection plus headroom for attempts that fail while the harness is
/// still settling on a loaded runner.
const REJECT_SERVE_CONNECTIONS: usize = 8;

/// Spawn `serve_registered_backend`, mirroring any serve error into a
/// channel so [`connect_backend_with_retry`] can fail fast with the real
/// cause instead of burning its whole deadline retrying against a broker
/// socket that was never bound (or was torn down on early exit).
fn spawn_serve(
    config: BrokerServeConfig,
) -> (
    thread::JoinHandle<Result<(), BrokerServeError>>,
    mpsc::Receiver<String>,
) {
    let (error_tx, error_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let result = serve_registered_backend(config);
        if let Err(err) = &result {
            let _ = error_tx.send(err.to_string());
        }
        result
    });
    (handle, error_rx)
}

/// Consume any bounded-accept slots left over after the asserted
/// connection, so the serve thread terminates and its result can be
/// asserted. Each drain dial is a full non-adopting Hello negotiation;
/// its reconnect lands on the still-running [`ReconnectBackend`] loop.
fn drain_serve_slots(
    server: &thread::JoinHandle<Result<(), BrokerServeError>>,
    broker_endpoint: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !server.is_finished() {
        assert!(
            Instant::now() < deadline,
            "timed out draining leftover broker serve slots on {broker_endpoint}"
        );
        let request = ConnectBackendRequest::new(broker_endpoint, "zccache", "1.11.20", "1.11.20");
        let _ = connect_to_backend(request);
        thread::sleep(Duration::from_millis(10));
    }
}

/// What the test backend does with the broker's handoff offer.
pub(crate) enum BackendBehavior {
    /// Echo-accept the offered token, adopt the transferred connection,
    /// and serve one probe/reply byte exchange on it.
    Accept,
    /// Reject the offer (expected-token mismatch) with a well-formed
    /// `accepted = false` ACK.
    Reject,
}

/// Bind the backend handoff endpoint and serve one offer/ACK exchange
/// using the production backend-side wire helpers.
fn spawn_backend_handoff_listener(
    handoff_endpoint: String,
    behavior: BackendBehavior,
) -> thread::JoinHandle<io::Result<()>> {
    let display_name = handoff_endpoint.clone();
    let (ready_tx, ready_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let listener = bind_ready_test_socket(&handoff_endpoint, &ready_tx)?;
        let mut stream = listener.accept()?;
        let result = serve_one_handoff(&mut stream, &behavior);
        cleanup_test_socket(&handoff_endpoint);
        result
    });
    await_test_socket_ready(&ready_rx, &display_name);
    handle
}

#[cfg(windows)]
pub(crate) fn serve_one_handoff(
    stream: &mut interprocess::local_socket::Stream,
    behavior: &BackendBehavior,
) -> io::Result<()> {
    serve_one_handoff_with_deadline(stream, behavior, Instant::now() + Duration::from_secs(5))
}

#[cfg(windows)]
fn serve_one_handoff_with_deadline(
    stream: &mut interprocess::local_socket::Stream,
    behavior: &BackendBehavior,
    deadline: Instant,
) -> io::Result<()> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};

    let offer = read_handoff_offer_with_deadline(stream, deadline).map_err(io::Error::other)?;
    let handle_value = offer.handle_value;
    let adopted = unsafe { OwnedHandle::from_raw_handle(handle_value as RawHandle) };
    respond(stream, behavior, offer)?;
    if matches!(behavior, BackendBehavior::Accept) {
        // Adopt the handle the broker duplicated into this (backend)
        // process and prove it serves the client's connection. The pipe
        // was created overlapped by the broker's listener, so the byte
        // exchange uses explicit OVERLAPPED I/O on the raw handle.
        let mut probe = [0_u8; 1];
        overlapped_transfer(adopted.as_raw_handle(), &mut probe, false)?;
        if probe != [CLIENT_PROBE] {
            return Err(io::Error::other(
                "unexpected probe byte on adopted connection",
            ));
        }
        overlapped_transfer(adopted.as_raw_handle(), &mut [BACKEND_REPLY], true)?;
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn timed_out_offer_read_closes_received_scm_rights_fd() {
    use std::os::fd::{AsFd, AsRawFd};
    use std::os::unix::net::UnixStream;

    use interprocess::local_socket::traits::Stream as _;
    use running_process::broker::server::handoff::{
        try_send_scm_rights_over, ScmRightsAttempt, UnixFileDescriptor, UnixHandoffSocket,
    };

    let socket_name = unique_socket_name("handoff-offer-fd-timeout");
    let listener = bind_test_socket(&socket_name).expect("bind handoff socket");
    let server = thread::spawn(move || {
        let mut stream = listener.accept().expect("accept handoff peer");
        serve_one_handoff_with_deadline(
            &mut stream,
            &BackendBehavior::Reject,
            Instant::now() + Duration::from_millis(50),
        )
    });

    let name = running_process::broker::server::local_socket_name(&socket_name)
        .expect("handoff socket name");
    let broker = interprocess::local_socket::Stream::connect(name).expect("connect handoff peer");
    let broker_fd = match &broker {
        interprocess::local_socket::Stream::UdSocket(socket) => socket.as_fd().as_raw_fd(),
    };
    let (passed, mut observer) = UnixStream::pair().expect("descriptor pair");
    let attempt = ScmRightsAttempt::new(
        UnixFileDescriptor::new(passed.as_raw_fd()),
        UnixHandoffSocket::new(&socket_name),
        HandoffToken::from_bytes([0x61; HANDOFF_TOKEN_BYTES]),
    );
    try_send_scm_rights_over(broker_fd, &attempt).expect("send SCM_RIGHTS descriptor");
    drop(passed);

    server
        .join()
        .expect("handoff server")
        .expect_err("silent offer must time out");
    drop(broker);
    cleanup_test_socket(&socket_name);

    observer
        .set_nonblocking(true)
        .expect("nonblocking observer");
    let closure_deadline = Instant::now() + Duration::from_secs(1);
    let mut byte = [0_u8; 1];
    loop {
        match observer.read(&mut byte) {
            Ok(0) => break,
            Ok(count) => panic!("unexpected {count} bytes on descriptor observer"),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < closure_deadline,
                    "received SCM_RIGHTS descriptor leaked after offer timeout"
                );
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("failed to observe descriptor closure: {error}"),
        }
    }
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
pub(crate) fn serve_one_handoff(
    stream: &mut interprocess::local_socket::Stream,
    behavior: &BackendBehavior,
) -> io::Result<()> {
    serve_one_handoff_with_deadline(stream, behavior, Instant::now() + Duration::from_secs(5))
}

#[cfg(unix)]
fn serve_one_handoff_with_deadline(
    stream: &mut interprocess::local_socket::Stream,
    behavior: &BackendBehavior,
    deadline: Instant,
) -> io::Result<()> {
    use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::net::UnixStream;

    // The fd plus token ride SCM_RIGHTS on the same handoff connection
    // that then carries the offer frame.
    let socket_fd = match &*stream {
        interprocess::local_socket::Stream::UdSocket(socket) => socket.as_fd().as_raw_fd(),
    };
    let (received_fd, _token) = recv_fd_and_token(socket_fd)?;
    let received_fd = unsafe { OwnedFd::from_raw_fd(received_fd) };
    let offer = read_handoff_offer_with_deadline(stream, deadline).map_err(io::Error::other)?;
    respond(stream, behavior, offer)?;
    match behavior {
        BackendBehavior::Accept => {
            let mut adopted = UnixStream::from(received_fd);
            serve_probe_reply(&mut adopted)?;
        }
        BackendBehavior::Reject => drop(received_fd),
    }
    Ok(())
}

/// Answer one offer through the production backend wire path: accepted
/// with the token seeded as pending, rejected via expected-token mismatch.
fn respond<S: Write>(
    stream: &mut S,
    behavior: &BackendBehavior,
    offer: HandoffOffer,
) -> io::Result<()> {
    let now = Instant::now();
    let mut pending_tokens = HandoffTokenStore::new();
    let expected_token = match behavior {
        BackendBehavior::Accept => {
            let bytes = <[u8; HANDOFF_TOKEN_BYTES]>::try_from(offer.token.as_slice())
                .map_err(|_| io::Error::other("offered token is not 16 bytes"))?;
            pending_tokens
                .issue_with_random128(now, || Ok(bytes))
                .map_err(io::Error::other)?
        }
        BackendBehavior::Reject => HandoffToken::from_bytes([0xEE; HANDOFF_TOKEN_BYTES]),
    };
    respond_to_handoff_offer(stream, &mut pending_tokens, expected_token, offer, now)
        .map_err(io::Error::other)?;
    Ok(())
}

#[cfg(unix)]
fn serve_probe_reply<S: Read + Write>(stream: &mut S) -> io::Result<()> {
    let mut probe = [0_u8; 1];
    stream.read_exact(&mut probe)?;
    if probe != [CLIENT_PROBE] {
        return Err(io::Error::other(
            "unexpected probe byte on adopted connection",
        ));
    }
    stream.write_all(&[BACKEND_REPLY])
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

/// Reconnect listener on the backend endpoint that accepts every dial —
/// the asserted downgrade reconnect plus any retried or drain
/// connections — until explicitly stopped.
///
/// A single-accept listener here is a race on a loaded runner: one stray
/// dial consumes the only accept and unlinks the socket, so every later
/// reconnect fails with ENOENT.
struct ReconnectBackend {
    endpoint: String,
    stop: Arc<AtomicBool>,
    handle: thread::JoinHandle<io::Result<()>>,
}

impl ReconnectBackend {
    /// Bind with retry while the just-closed startup-probe listener still
    /// holds the pipe name, then accept in a loop until [`Self::stop`].
    fn spawn(backend_endpoint: String) -> Self {
        let display_name = backend_endpoint.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let endpoint = backend_endpoint.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let listener = loop {
                match bind_test_socket(&backend_endpoint) {
                    Ok(listener) => break listener,
                    Err(error) if Instant::now() < deadline => {
                        let _ = error;
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error.to_string()));
                        return Err(error);
                    }
                }
            };
            ready_tx.send(Ok(())).unwrap();
            while !thread_stop.load(Ordering::SeqCst) {
                drop(listener.accept()?);
            }
            cleanup_test_socket(&backend_endpoint);
            Ok(())
        });
        await_test_socket_ready(&ready_rx, &display_name);
        Self {
            endpoint,
            stop,
            handle,
        }
    }

    /// Signal shutdown, wake the blocked accept with one throwaway dial,
    /// and join the listener thread.
    fn stop(self) -> io::Result<()> {
        self.stop.store(true, Ordering::SeqCst);
        let _ = connect_local_socket(&self.endpoint);
        self.handle.join().unwrap()
    }
}

/// Opted-in client connect, retrying only while the broker socket is not
/// yet bound. Each successful dial performs one full Hello negotiation.
///
/// Fails fast with the real serve error when the broker thread has
/// already exited — otherwise that error stays trapped in its un-joined
/// handle while this loop burns the full deadline on opaque connection
/// refusals. Every distinct client error seen across the retry loop is
/// recorded (with counts and first-seen offsets) and replayed in the
/// panic message, so a deadline failure names the whole error sequence —
/// e.g. an early `Refused` burst followed by an ENOENT tail proves the
/// broker bound, burned its bounded accept slots silently, and unlinked
/// its socket — instead of only the final opaque error (#399).
fn connect_backend_with_retry(
    broker_endpoint: &str,
    ready_timeout: Duration,
    server: &thread::JoinHandle<Result<(), BrokerServeError>>,
    serve_errors: &mpsc::Receiver<String>,
) -> BackendConnection {
    let started = Instant::now();
    let deadline = started + Duration::from_secs(15);
    let mut history = ErrorHistory::default();
    loop {
        let mut request =
            ConnectBackendRequest::new(broker_endpoint, "zccache", "1.11.20", "1.11.20");
        request.adopt_handed_off_connection = true;
        request.handoff_ready_timeout = ready_timeout;
        match connect_to_backend(request) {
            Ok(connection) => return connection,
            Err(err) => {
                history.record(err.to_string(), started.elapsed());
                if let Ok(serve_error) = serve_errors.try_recv() {
                    panic!(
                        "broker serve loop on {broker_endpoint} failed: {serve_error}\n{}",
                        history.render()
                    );
                }
                if server.is_finished() {
                    panic!(
                        "broker serve thread on {broker_endpoint} exited (Ok) before the \
                         client succeeded — bounded accept slots likely consumed \
                         silently\n{}",
                        history.render()
                    );
                }
                if Instant::now() >= deadline {
                    panic!(
                        "timed out connecting through broker {broker_endpoint}\n{}",
                        history.render()
                    );
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// Deduplicated client-error log for [`connect_backend_with_retry`]:
/// distinct error messages in first-seen order, each with an occurrence
/// count and the elapsed offset of its first sighting.
#[derive(Default)]
struct ErrorHistory {
    entries: Vec<(String, usize, Duration)>,
}

impl ErrorHistory {
    fn record(&mut self, message: String, elapsed: Duration) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|(seen, _, _)| *seen == message)
        {
            entry.1 += 1;
        } else {
            self.entries.push((message, 1, elapsed));
        }
    }

    fn render(&self) -> String {
        let mut out = String::from("client error history (first-seen order):");
        for (message, count, first_seen) in &self.entries {
            out.push_str(&format!(
                "\n  [x{count}, first at {:.3}s] {message}",
                first_seen.as_secs_f64()
            ));
        }
        out
    }
}

pub(crate) fn serve_config(
    service_root: &Path,
    socket_path: String,
    backend_endpoint: String,
    max_connections: usize,
) -> BrokerServeConfig {
    BrokerServeConfig::new(
        socket_path,
        "zccache",
        "1.11.20",
        backend_endpoint,
        max_connections,
    )
    .unwrap()
    .with_service_definition_dir(service_root)
}

pub(crate) fn spawn_configured_backend_probe(
    backend_endpoint: &str,
) -> thread::JoinHandle<io::Result<()>> {
    let endpoint = Endpoint {
        namespace_id: BrokerInstanceKey::Shared.id(),
        path: backend_endpoint.into(),
    };
    let daemon = DaemonProcess::current_process(endpoint, Some(30)).unwrap();
    spawn_endpoint_probe_once(daemon)
}

fn service_definition() -> ServiceDefinition {
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().to_path_buf();
    ServiceDefinition {
        service_name: "zccache".into(),
        binary_path: exe.to_string_lossy().into_owned(),
        isolation: BrokerIsolation::SharedBroker as i32,
        explicit_instance: String::new(),
        per_version_binary_dir: dir.to_string_lossy().into_owned(),
        min_version: "1.10.0".into(),
        version_allow_list: vec!["1.11.20".into()],
        labels: Default::default(),
    }
}

pub(crate) fn write_service_definition_dir() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("services");
    ensure_service_definition_dir(&root).unwrap();
    let path = service_definition_path(&root, "zccache").unwrap();
    fs::write(path, service_definition().encode_to_vec()).unwrap();
    tmp
}
