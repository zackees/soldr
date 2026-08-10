#![cfg(feature = "client")]

//! Real-path latency evidence for the orchestrated handoff vs the
//! reconnect fallback (#354, slice 5).
//!
//! Two operations are measured with monotonic clocks
//! ([`collect_latency_samples`] warmup + N iterations, P50/P99 via
//! [`summarize_latency_samples`]):
//!
//! - **handoff**: the completed production orchestration from #367/#368 —
//!   one-time token issue + ACK registration, the real platform transport
//!   (`DuplicateHandle` into a live child process on Windows via the
//!   child-helper protocol, `sendmsg(SCM_RIGHTS)` over a real
//!   `UnixListener` on Unix), payload adoption by the backend, and the
//!   token-echo acknowledgement completing the registry entry.
//! - **reconnect**: the fallback the handoff replaces — the client
//!   connects to the cached `backend_pipe` afresh through
//!   [`connect_to_backend`] (Hello-skip route, local-socket connect) and
//!   writes the same payload.
//!
//! The tests are deterministically green: they assert the harness produced
//! sane samples (all collected, non-zero, P50 <= P99) and PRINT the
//! measured P50/P99 numbers so each run leaves latency evidence in the
//! test output. They intentionally do NOT assert "handoff is faster" —
//! that comparison would flake under CI scheduler noise. Measured numbers
//! are recorded in `docs/v1-handoff-optimization.md`.

use std::io::{self, Read, Write};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use interprocess::local_socket::prelude::*;
use interprocess::local_socket::ListenerOptions;
use running_process::broker::client::{
    connect_to_backend, BackendConnectionRoute, ConnectBackendRequest,
};
use running_process::broker::server::handoff::{
    collect_latency_samples, summarize_latency_samples, HandoffLatencySummary,
};
use running_process::broker::server::local_socket_name;

use crate::socket_common::unique_socket_name;

const WARMUP_ITERATIONS: usize = 5;
const MEASURED_ITERATIONS: usize = 50;
const CLIENT_PAYLOAD: &[u8] = b"running-process handoff latency probe";

/// Time one reconnect-fallback round: connect to the cached backend
/// endpoint afresh (Hello-skip route) and push the client payload.
///
/// Listener bind and accept-thread spawn happen outside the timed region;
/// only the client-visible reconnect cost (local-socket connect + first
/// payload write) is measured, mirroring `hello_skip.rs`.
fn reconnect_fallback_sample() -> Duration {
    let endpoint = unique_socket_name("lat-reconnect");
    let backend = spawn_accept_once(endpoint.clone());

    let started = Instant::now();
    let mut request = ConnectBackendRequest::new("missing-broker", "zccache", "1.11.20", "1.11.20");
    request.cached_backend_endpoint = Some(&endpoint);
    let mut connection = connect_to_backend(request).unwrap();
    connection.stream.write_all(CLIENT_PAYLOAD).unwrap();
    let elapsed = started.elapsed();

    assert_eq!(connection.route, BackendConnectionRoute::HelloSkip);
    drop(connection);
    backend.join().unwrap().unwrap();
    elapsed
}

fn measure_reconnect_fallback() -> HandoffLatencySummary {
    let samples = collect_latency_samples(
        WARMUP_ITERATIONS,
        MEASURED_ITERATIONS,
        reconnect_fallback_sample,
    );
    assert_sane(&samples, "reconnect")
}

/// Sanity gate shared by both benchmarks: all iterations produced a
/// sample, every sample is non-zero, and the percentiles are ordered.
fn assert_sane(samples: &[Duration], label: &str) -> HandoffLatencySummary {
    assert_eq!(
        samples.len(),
        MEASURED_ITERATIONS,
        "{label} benchmark must collect every measured iteration"
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

fn print_evidence(
    platform: &str,
    handoff: HandoffLatencySummary,
    reconnect: HandoffLatencySummary,
) {
    println!(
        "handoff-latency[{platform}]: handoff p50={}us p99={}us (n={}) \
         reconnect p50={}us p99={}us (n={})",
        handoff.p50.as_micros(),
        handoff.p99.as_micros(),
        handoff.sample_count,
        reconnect.p50.as_micros(),
        reconnect.p99.as_micros(),
        reconnect.sample_count,
    );
}

fn spawn_accept_once(socket_name: String) -> thread::JoinHandle<io::Result<()>> {
    let display_name = socket_name.clone();
    let (ready_tx, ready_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let listener = match bind_test_socket(&socket_name) {
            Ok(listener) => {
                ready_tx.send(Ok(())).unwrap();
                listener
            }
            Err(err) => {
                let _ = ready_tx.send(Err(err.to_string()));
                return Err(err);
            }
        };
        let mut stream = listener.accept()?;
        let mut payload = vec![0_u8; CLIENT_PAYLOAD.len()];
        stream.read_exact(&mut payload)?;
        assert_eq!(payload, CLIENT_PAYLOAD);
        cleanup_test_socket(&socket_name);
        Ok(())
    });
    match ready_rx.recv_timeout(Duration::from_secs(3)) {
        Ok(Ok(())) => {}
        Ok(Err(err)) => panic!("failed to bind latency backend socket {display_name}: {err}"),
        Err(err) => panic!("timed out waiting for latency backend socket readiness: {err}"),
    }
    handle
}

fn bind_test_socket(socket_name: &str) -> io::Result<interprocess::local_socket::Listener> {
    prepare_test_socket(socket_name)?;
    let name = local_socket_name(socket_name)?;
    ListenerOptions::new().name(name).create_sync()
}

fn prepare_test_socket(socket_name: &str) -> io::Result<()> {
    #[cfg(unix)]
    {
        let path = std::path::Path::new(socket_name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _ = std::fs::remove_file(path);
    }

    #[cfg(windows)]
    let _ = socket_name;

    Ok(())
}

fn cleanup_test_socket(socket_name: &str) {
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(socket_name);
    }

    #[cfg(windows)]
    let _ = socket_name;
}

#[cfg(windows)]
fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

// ---------------------------------------------------------------------------
// Unix: real SCM_RIGHTS orchestration vs reconnect.
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod unix_bench {
    use super::*;

    use std::os::fd::{AsRawFd, FromRawFd, RawFd};
    use std::os::unix::net::{UnixListener, UnixStream};

    use running_process::broker::server::handoff::{
        execute_unix_handoff, HandoffAckRegistry, HandoffDeliveryError, HandoffToken,
        HandoffTokenStore, PendingHandoffBackend, UnixFileDescriptor, UnixHandoffAckWait,
        UnixHandoffRequest, UnixHandoffSocket,
    };

    /// ACK observed once the backend thread has adopted the connection.
    struct BackendEcho {
        token: HandoffToken,
        payload: Vec<u8>,
    }

    /// ACK channel fed by the in-process backend thread (one echo per
    /// handoff iteration).
    struct ChannelAckWait {
        receiver: mpsc::Receiver<BackendEcho>,
    }

    impl UnixHandoffAckWait for ChannelAckWait {
        fn await_backend_ack(
            &mut self,
            token: &HandoffToken,
            deadline: Instant,
        ) -> Result<Instant, HandoffDeliveryError> {
            let timeout = deadline.saturating_duration_since(Instant::now());
            let echo = self.receiver.recv_timeout(timeout).map_err(|err| {
                HandoffDeliveryError::AckNotObserved {
                    detail: format!("backend echo not received: {err}"),
                }
            })?;
            if echo.token != *token {
                return Err(HandoffDeliveryError::AckNotObserved {
                    detail: "backend echoed a different token".into(),
                });
            }
            assert_eq!(
                echo.payload, CLIENT_PAYLOAD,
                "backend must read the client payload through the received fd"
            );
            Ok(Instant::now())
        }
    }

    /// Receive one `SCM_RIGHTS` message: the 16-byte token plus one fd.
    fn recv_fd_and_token(stream: &UnixStream) -> (RawFd, HandoffToken) {
        let mut token_payload = [0_u8; 16];
        let mut iov = libc::iovec {
            iov_base: token_payload.as_mut_ptr().cast(),
            iov_len: token_payload.len(),
        };
        let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as _) as usize };
        let mut control = vec![0_u8; space];
        let mut message = unsafe { std::mem::zeroed::<libc::msghdr>() };
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = control.len() as _;

        let received = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut message, 0) };
        assert_eq!(received as usize, token_payload.len());

        let header = unsafe { libc::CMSG_FIRSTHDR(&message) };
        assert!(!header.is_null());
        unsafe {
            assert_eq!((*header).cmsg_level, libc::SOL_SOCKET);
            assert_eq!((*header).cmsg_type, libc::SCM_RIGHTS);
            let received_fd = *libc::CMSG_DATA(header).cast::<libc::c_int>();
            (received_fd, HandoffToken::from_bytes(token_payload))
        }
    }

    #[test]
    fn unix_handoff_vs_reconnect_latency_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("handoff-latency.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();

        // Long-lived "backend": one accept + fd adoption + token echo per
        // handoff iteration, mirroring a backend that stays resident.
        let total = WARMUP_ITERATIONS + MEASURED_ITERATIONS;
        let (ack_tx, ack_rx) = mpsc::channel::<BackendEcho>();
        let backend = thread::spawn(move || {
            for _ in 0..total {
                let (stream, _) = listener.accept().unwrap();
                let (received_fd, received_token) = recv_fd_and_token(&stream);
                let mut adopted = unsafe { UnixStream::from_raw_fd(received_fd) };
                let mut payload = vec![0_u8; CLIENT_PAYLOAD.len()];
                adopted.read_exact(&mut payload).unwrap();
                ack_tx
                    .send(BackendEcho {
                        token: received_token,
                        payload,
                    })
                    .unwrap();
            }
        });

        let mut tokens = HandoffTokenStore::new();
        let mut acks = HandoffAckRegistry::new();
        let mut ack_wait = ChannelAckWait { receiver: ack_rx };
        let handoff_samples =
            collect_latency_samples(WARMUP_ITERATIONS, MEASURED_ITERATIONS, || {
                // Per-iteration setup outside the timed region: the
                // handed-off client connection already exists when the
                // broker decides to hand it off.
                let (mut client_end, broker_held_conn) = UnixStream::pair().unwrap();
                client_end.write_all(CLIENT_PAYLOAD).unwrap();

                // Timed region: token issue + ACK registration + the full
                // orchestrated SCM_RIGHTS handoff through to the ACK.
                let started = Instant::now();
                let issued = tokens.issue(started).unwrap();
                acks.register(
                    issued,
                    PendingHandoffBackend::new("zccache", std::process::id()),
                    started,
                );
                let outcome = execute_unix_handoff(
                    &mut tokens,
                    &mut acks,
                    &UnixHandoffRequest::new(
                        UnixFileDescriptor::new(broker_held_conn.as_raw_fd()),
                        UnixHandoffSocket::new(&socket_path),
                        issued,
                    ),
                    &mut ack_wait,
                );
                let elapsed = started.elapsed();
                assert!(
                    outcome.is_completed(),
                    "latency iteration must complete the handoff, got {outcome:?}"
                );
                elapsed
            });
        backend.join().unwrap();

        let handoff = assert_sane(&handoff_samples, "handoff");
        let reconnect = measure_reconnect_fallback();
        print_evidence("unix", handoff, reconnect);
    }
}

// ---------------------------------------------------------------------------
// Windows: real DuplicateHandle orchestration vs reconnect.
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod windows_bench {
    use super::*;

    use std::fs;
    use std::io::{BufRead, BufReader};
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::path::{Path, PathBuf};
    use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

    use running_process::broker::backend_handle::{BackendHandle, DaemonProcess};
    use running_process::broker::backend_lifecycle::probe::handle_endpoint_probe;
    use running_process::broker::protocol::Endpoint;
    use running_process::broker::server::handoff::{
        execute_verified_windows_handoff, HandoffAckRegistry, HandoffDelivery,
        HandoffDeliveryError, HandoffToken, HandoffTokenStore, PendingHandoffBackend,
        WindowsHandleValue, HANDOFF_TOKEN_BYTES,
    };
    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
    use windows_sys::Win32::System::Pipes::CreatePipe;

    const CHILD_HELPER_ENV: &str = "RUNNING_PROCESS_HANDOFF_LATENCY_CHILD";
    const CHILD_ENDPOINT_ENV: &str = "RUNNING_PROCESS_HANDOFF_LATENCY_ENDPOINT";
    const CHILD_READY_FILE_ENV: &str = "RUNNING_PROCESS_HANDOFF_LATENCY_READY_FILE";
    const CHILD_HELPER_TEST: &str =
        "handoff_latency_e2e::windows_bench::windows_latency_child_helper";
    const CHILD_ACK_MARKER: &str = "running-process-latency-handoff-ack";

    /// Delivery channel over the long-lived child-helper line protocol.
    ///
    /// `deliver` writes one `(handle value, token, payload length)` manifest
    /// line to the child's stdin and pushes the client payload through the
    /// broker-held write end of the per-iteration pipe; `await_backend_ack`
    /// reads child stdout lines until the exact token is echoed back.
    struct LineProtocolDelivery {
        stdin: Option<ChildStdin>,
        stdout: BufReader<ChildStdout>,
        write_pipe: Option<OwnedHandle>,
    }

    impl HandoffDelivery for LineProtocolDelivery {
        fn deliver(
            &mut self,
            handle: WindowsHandleValue,
            token: &HandoffToken,
        ) -> Result<(), HandoffDeliveryError> {
            let manifest = format!(
                "{} {} {}\n",
                handle.get(),
                bytes_to_hex(token.as_bytes()),
                CLIENT_PAYLOAD.len()
            );
            let stdin =
                self.stdin
                    .as_mut()
                    .ok_or_else(|| HandoffDeliveryError::DeliveryFailed {
                        detail: "child stdin already closed".into(),
                    })?;
            stdin
                .write_all(manifest.as_bytes())
                .and_then(|()| stdin.flush())
                .map_err(|err| HandoffDeliveryError::DeliveryFailed {
                    detail: format!("manifest write failed: {err}"),
                })?;

            let write_pipe =
                self.write_pipe
                    .take()
                    .ok_or_else(|| HandoffDeliveryError::DeliveryFailed {
                        detail: "write pipe already consumed".into(),
                    })?;
            let mut written = 0;
            let write_ok = unsafe {
                WriteFile(
                    write_pipe.as_raw_handle() as HANDLE,
                    CLIENT_PAYLOAD.as_ptr().cast(),
                    CLIENT_PAYLOAD.len() as u32,
                    &mut written,
                    std::ptr::null_mut(),
                )
            };
            drop(write_pipe);
            if write_ok == 0 || written as usize != CLIENT_PAYLOAD.len() {
                return Err(HandoffDeliveryError::DeliveryFailed {
                    detail: "payload write through broker pipe failed".into(),
                });
            }
            Ok(())
        }

        fn await_backend_ack(
            &mut self,
            token: &HandoffToken,
            _deadline: Instant,
        ) -> Result<Instant, HandoffDeliveryError> {
            let expected = format!(
                "{CHILD_ACK_MARKER} token={}",
                bytes_to_hex(token.as_bytes())
            );
            loop {
                let mut line = String::new();
                let read = self.stdout.read_line(&mut line).map_err(|err| {
                    HandoffDeliveryError::AckNotObserved {
                        detail: format!("reading child ack line failed: {err}"),
                    }
                })?;
                if read == 0 {
                    return Err(HandoffDeliveryError::AckNotObserved {
                        detail: "child stdout closed before the token was echoed".into(),
                    });
                }
                // The libtest harness writes its own preamble lines around
                // the helper; skip anything that is not the token echo.
                if line.contains(&expected) {
                    return Ok(Instant::now());
                }
            }
        }
    }

    #[test]
    fn windows_handoff_vs_reconnect_latency_evidence() {
        let endpoint = child_endpoint();
        let ready_file = child_ready_file();
        let _ = fs::remove_file(&ready_file);

        // Long-lived "backend": one child process serving every handoff
        // iteration, mirroring a backend that stays resident.
        let mut child = spawn_child(&endpoint.path, &ready_file);
        let child_pid = child.id();
        wait_for_ready_file(&ready_file);

        let daemon = daemon_for_child(child_pid, endpoint.clone());
        let backend =
            BackendHandle::probe_with_service("zccache", "1.11.20", &endpoint, &daemon).unwrap();
        assert_eq!(backend.daemon_process.pid, child_pid);

        let mut delivery = LineProtocolDelivery {
            stdin: Some(child.stdin.take().expect("child stdin pipe")),
            stdout: BufReader::new(child.stdout.take().expect("child stdout pipe")),
            write_pipe: None,
        };
        let mut tokens = HandoffTokenStore::new();
        let mut acks = HandoffAckRegistry::new();

        let handoff_samples =
            collect_latency_samples(WARMUP_ITERATIONS, MEASURED_ITERATIONS, || {
                // Per-iteration setup outside the timed region: the
                // broker-held client pipe already exists when the broker
                // decides to hand it off.
                let (read_pipe, write_pipe) = create_pipe_pair();
                delivery.write_pipe = Some(write_pipe);

                // Timed region: token issue + ACK registration + the full
                // orchestrated DuplicateHandle handoff through to the ACK.
                let started = Instant::now();
                let token = tokens.issue(started).unwrap();
                acks.register(
                    token,
                    PendingHandoffBackend::new("zccache", child_pid),
                    started,
                );
                let outcome = execute_verified_windows_handoff(
                    &backend,
                    WindowsHandleValue::new(read_pipe.as_raw_handle() as usize),
                    token,
                    &mut tokens,
                    &mut acks,
                    &mut delivery,
                );
                let elapsed = started.elapsed();
                assert!(
                    outcome.is_completed(),
                    "latency iteration must complete the handoff, got {outcome:?}"
                );
                drop(read_pipe);
                elapsed
            });

        // Closing stdin ends the child helper's manifest loop; drain stdout
        // to EOF so the harness epilogue never hits a closed pipe.
        drop(delivery.stdin.take());
        let mut epilogue = Vec::new();
        delivery
            .stdout
            .read_to_end(&mut epilogue)
            .expect("child stdout must drain to EOF");
        let status = child.wait().expect("child helper must exit");
        assert!(status.success(), "child helper must exit cleanly");
        let _ = fs::remove_file(&ready_file);

        let handoff = assert_sane(&handoff_samples, "handoff");
        let reconnect = measure_reconnect_fallback();
        print_evidence("windows", handoff, reconnect);
    }

    /// Long-lived child helper: answer the endpoint identity probe once,
    /// then adopt one duplicated handle per stdin manifest line, read the
    /// payload through it, and echo the paired token until stdin closes.
    #[test]
    #[ignore = "spawned by windows_handoff_vs_reconnect_latency_evidence"]
    fn windows_latency_child_helper() {
        if std::env::var_os(CHILD_HELPER_ENV).is_none() {
            return;
        }

        if let Some(endpoint_path) = std::env::var_os(CHILD_ENDPOINT_ENV) {
            serve_child_endpoint_probe_once(&endpoint_path.to_string_lossy());
        }

        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let line = line.expect("child helper must read stdin manifest line");
            if line.trim().is_empty() {
                continue;
            }
            let manifest = ChildManifest::parse(&line);
            let handle = manifest.duplicated_handle as HANDLE;
            assert_valid_handle(handle);
            let token = parse_token_hex(&manifest.token_hex);

            let mut buffer = vec![0_u8; manifest.expected_len];
            let mut total_read = 0;
            while total_read < buffer.len() {
                let mut bytes_read = 0;
                let remaining = &mut buffer[total_read..];
                let read_ok = unsafe {
                    ReadFile(
                        handle,
                        remaining.as_mut_ptr().cast(),
                        remaining.len() as u32,
                        &mut bytes_read,
                        std::ptr::null_mut(),
                    )
                };
                assert_ne!(read_ok, 0, "ReadFile must read the duplicated pipe handle");
                assert_ne!(bytes_read, 0, "pipe closed before payload was fully read");
                total_read += bytes_read as usize;
            }
            assert_eq!(buffer, CLIENT_PAYLOAD);
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(handle);
            }

            // Stdout is a LineWriter; the trailing newline flushes the ack.
            let ack = format!("{CHILD_ACK_MARKER} token={}\n", bytes_to_hex(&token));
            std::io::stdout()
                .write_all(ack.as_bytes())
                .expect("child helper must write ack line");
        }
    }

    fn spawn_child(endpoint_path: &str, ready_file: &Path) -> Child {
        Command::new(std::env::current_exe().expect("test binary path"))
            .args([
                "--ignored",
                "--exact",
                CHILD_HELPER_TEST,
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_HELPER_ENV, "1")
            .env(CHILD_ENDPOINT_ENV, endpoint_path)
            .env(CHILD_READY_FILE_ENV, ready_file)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("must spawn latency handoff child helper")
    }

    fn create_pipe_pair() -> (OwnedHandle, OwnedHandle) {
        let mut read_pipe: HANDLE = std::ptr::null_mut();
        let mut write_pipe: HANDLE = std::ptr::null_mut();
        let created =
            unsafe { CreatePipe(&mut read_pipe, &mut write_pipe, std::ptr::null_mut(), 0) };
        assert_ne!(created, 0, "CreatePipe must create a real pipe pair");
        assert_valid_handle(read_pipe);
        assert_valid_handle(write_pipe);
        unsafe {
            (
                OwnedHandle::from_raw_handle(read_pipe.cast()),
                OwnedHandle::from_raw_handle(write_pipe.cast()),
            )
        }
    }

    fn child_endpoint() -> Endpoint {
        Endpoint {
            namespace_id: "verified-child".into(),
            path: format!(
                r"\\.\pipe\rpb-v1-lat-{}-{}",
                std::process::id(),
                unique_suffix()
            ),
        }
    }

    fn child_ready_file() -> PathBuf {
        std::env::temp_dir().join(format!(
            "running-process-handoff-latency-ready-{}-{}",
            std::process::id(),
            unique_suffix()
        ))
    }

    fn daemon_for_child(pid: u32, ipc_endpoint: Endpoint) -> DaemonProcess {
        let mut daemon = DaemonProcess::current_process(ipc_endpoint, Some(30)).unwrap();
        daemon.pid = pid;
        daemon
    }

    fn serve_child_endpoint_probe_once(endpoint_path: &str) {
        let endpoint = Endpoint {
            namespace_id: "verified-child".into(),
            path: endpoint_path.into(),
        };
        let daemon = DaemonProcess::current_process(endpoint.clone(), Some(30)).unwrap();
        let name = local_socket_name(&endpoint.path).unwrap();
        let listener = ListenerOptions::new()
            .name(name)
            .create_sync()
            .expect("child helper must bind endpoint probe socket");
        if let Some(ready_file) = std::env::var_os(CHILD_READY_FILE_ENV) {
            fs::write(PathBuf::from(ready_file), b"ready")
                .expect("child helper must write ready file");
        }
        let mut stream = listener
            .accept()
            .expect("child helper must accept endpoint probe");
        handle_endpoint_probe(&mut stream, &daemon)
            .expect("child helper must answer endpoint probe");
    }

    fn wait_for_ready_file(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if path.exists() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("child helper did not report endpoint readiness at {path:?}");
    }

    struct ChildManifest {
        duplicated_handle: usize,
        token_hex: String,
        expected_len: usize,
    }

    impl ChildManifest {
        fn parse(input: &str) -> Self {
            let mut fields = input.split_whitespace();
            let duplicated_handle = fields
                .next()
                .expect("manifest handle")
                .parse()
                .expect("manifest handle must be usize");
            let token_hex = fields.next().expect("manifest token").to_owned();
            let expected_len = fields
                .next()
                .expect("manifest expected length")
                .parse()
                .expect("manifest expected length must be usize");
            assert!(
                fields.next().is_none(),
                "manifest has unexpected trailing fields"
            );
            Self {
                duplicated_handle,
                token_hex,
                expected_len,
            }
        }
    }

    fn bytes_to_hex(bytes: &[u8]) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut encoded = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            encoded.push(HEX[(byte >> 4) as usize] as char);
            encoded.push(HEX[(byte & 0x0f) as usize] as char);
        }
        encoded
    }

    fn parse_token_hex(hex: &str) -> [u8; HANDOFF_TOKEN_BYTES] {
        assert_eq!(hex.len(), HANDOFF_TOKEN_BYTES * 2);
        let mut token = [0_u8; HANDOFF_TOKEN_BYTES];
        for index in 0..HANDOFF_TOKEN_BYTES {
            token[index] = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
                .expect("token hex must be valid");
        }
        token
    }

    fn assert_valid_handle(handle: HANDLE) {
        assert!(!handle.is_null());
        assert_ne!(handle, INVALID_HANDLE_VALUE);
    }
}
