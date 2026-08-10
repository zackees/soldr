//! v2 broker client (slice 4 of #488).
//!
//! Counterpart of [`super::client`]. Single public entry point
//! [`connect`]: dial the v2 broker pipe by program name, exchange a
//! Hello / Negotiated, return a [`ClientSession`] handle.
//!
//! The v2 broker fronts each program via the namespace defined by
//! [`super::lifecycle::names_v2::v2_program_pipe`]. The Hello round-trip
//! itself reuses v1's framing (`protocol::{read_frame, write_frame}`)
//! and message shapes (`Hello`, `HelloReply`) per #470's coexistence
//! table. Subsequent slices add post-Hello operations (streaming,
//! HTTP endpoint discovery, etc.); this slice exposes only the
//! handshake so downstream consumers (zccache et al.) can pin against
//! a stable v2 client API while the broker side grows under them.

use std::io::{Read, Write};
use std::time::Duration;

use interprocess::local_socket::traits::Stream as _;
use interprocess::local_socket::Stream;
use prost::Message;

/// Default deadline for the Hello round-trip in [`connect`].
///
/// Mirrors v1's `AsyncBrokerSession::adopt` budget (~3s). A v2 broker
/// that accepts the dial but stalls (deadlock, GC pause, hung backend
/// resolver, ENOSPC log write) would otherwise hang the caller
/// indefinitely — local-socket streams have no portable read deadline,
/// so the only bound is via a helper thread + `recv_timeout`. Fixes
/// #517.
pub const DEFAULT_HELLO_DEADLINE: Duration = Duration::from_secs(3);

use crate::broker::lifecycle::names::PipePathError;
use crate::broker::lifecycle::names_v2::v2_program_pipe;
use crate::broker::lifecycle::sid::{user_sid_hash, SidError};
use crate::broker::protocol::{
    hello_reply, read_frame, write_frame, FramingError, Hello, HelloReply, Negotiated, Refused,
    ENVELOPE_VERSION,
};

/// Errors surfaced by [`connect`].
#[derive(Debug, thiserror::Error)]
pub enum BrokerV2Error {
    /// `user_sid_hash` failed.
    #[error(transparent)]
    Sid(#[from] SidError),

    /// Building the v2 pipe name failed.
    #[error(transparent)]
    PipeName(#[from] PipePathError),

    /// Dialing the v2 broker pipe failed (no listener, permission denied, ...).
    #[error("dial v2 broker pipe at {socket_path:?}: {source}")]
    Dial {
        /// Path the client attempted to dial.
        socket_path: String,
        /// Underlying IO error.
        #[source]
        source: std::io::Error,
    },

    /// Framing-layer error on read or write (envelope version mismatch,
    /// truncated body, oversized frame, ...).
    #[error(transparent)]
    Framing(#[from] FramingError),

    /// Underlying IO failure during Hello / HelloReply exchange.
    #[error("Hello round-trip io: {0}")]
    Io(#[from] std::io::Error),

    /// `HelloReply` payload failed to decode.
    #[error("HelloReply decode: {0}")]
    Decode(#[from] prost::DecodeError),

    /// `HelloReply` was syntactically valid but missing its `result` oneof.
    #[error("HelloReply.result missing")]
    MissingResult,

    /// Broker explicitly refused the Hello (returned a `Refused` reply).
    ///
    /// `retry_after_ms` is promoted from `details.retry_after_ms` to a
    /// top-level field so RateLimited callers don't have to thread the
    /// boxed prost payload back out to honor broker-supplied backoff.
    /// Matches the shape of v1's `BrokerClientError::Refused`. Fixes
    /// #518. `details` is kept so any future scalar / nested field in
    /// the prost message stays accessible without another API break.
    #[error("broker refused Hello: {reason}")]
    Refused {
        /// Human-readable refusal text.
        reason: String,
        /// Suggested back-off before retrying (0 = no hint). Mirrors the
        /// proto wire type (`Refused.retry_after_ms` is `uint64`).
        retry_after_ms: u64,
        /// Decoded refused payload for further inspection by callers.
        details: Box<Refused>,
    },

    /// Encoding the outbound `Hello` failed.
    #[error("Hello encode: {0}")]
    Encode(#[from] prost::EncodeError),
}

/// A live session with the v2 broker.
///
/// Wraps the underlying [`Stream`] plus the broker's [`Negotiated`]
/// reply. Future slices add operations on top (streaming frames, HTTP
/// endpoint discovery, etc.); slice 4 exposes only the handshake
/// result so downstream consumers can pin the API shape now.
#[derive(Debug)]
pub struct ClientSession {
    stream: Stream,
    negotiated: Negotiated,
}

impl ClientSession {
    /// The broker's negotiated reply to our `Hello`.
    pub fn negotiated(&self) -> &Negotiated {
        &self.negotiated
    }

    /// Consume the session into the raw byte stream + negotiated reply.
    ///
    /// Slices that add post-handshake operations build them on this
    /// raw stream until the v2 client surface stabilizes.
    pub fn into_inner(self) -> (Stream, Negotiated) {
        (self.stream, self.negotiated)
    }
}

/// Dial the v2 broker for `program` and exchange Hello / Negotiated.
///
/// Computes the pipe name via [`v2_program_pipe`], dials it, sends a
/// Hello carrying `program` as `service_name` and `version_hint` as
/// `wanted_version`, reads the HelloReply, and either returns a
/// [`ClientSession`] (on `Negotiated`) or a [`BrokerV2Error::Refused`]
/// (on `Refused`).
///
/// `connection_id` on the outbound Hello is left at 0 — the broker
/// assigns one and echoes it in the Negotiated reply.
///
/// Bounded by [`DEFAULT_HELLO_DEADLINE`]; for a custom deadline use
/// [`connect_with_deadline`].
pub fn connect(program: &str, version_hint: &str) -> Result<ClientSession, BrokerV2Error> {
    connect_with_deadline(program, version_hint, DEFAULT_HELLO_DEADLINE)
}

/// Same as [`connect`] but with a caller-supplied deadline for the
/// Hello round-trip. On deadline returns
/// `BrokerV2Error::Io(ErrorKind::TimedOut)` and the helper thread
/// continues to drain (there is no portable way to cancel a sync
/// `Stream::connect` / framed read mid-call).
///
/// Fixes #517 — without this bound, a v2 broker that accepts the dial
/// then stalls hangs the caller indefinitely.
pub fn connect_with_deadline(
    program: &str,
    version_hint: &str,
    deadline: Duration,
) -> Result<ClientSession, BrokerV2Error> {
    let program = program.to_owned();
    let version_hint = version_hint.to_owned();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(connect_unbounded(&program, &version_hint));
    });
    match rx.recv_timeout(deadline) {
        Ok(result) => result,
        Err(_) => Err(BrokerV2Error::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("v2 broker Hello did not complete within {deadline:?}"),
        ))),
    }
}

/// Inner connect without a deadline. Called from inside the helper
/// thread spawned by [`connect_with_deadline`].
fn connect_unbounded(program: &str, version_hint: &str) -> Result<ClientSession, BrokerV2Error> {
    let sid = user_sid_hash()?;
    let pipe_name = v2_program_pipe(program, &sid, 0)?;
    let socket_path = resolve_socket_path(&pipe_name);
    let name = wrap_socket_name(&socket_path).map_err(|err| BrokerV2Error::Dial {
        socket_path: socket_path.clone(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, err),
    })?;
    let mut stream = Stream::connect(name).map_err(|source| BrokerV2Error::Dial {
        socket_path: socket_path.clone(),
        source,
    })?;
    let negotiated = hello_round_trip(&mut stream, program, version_hint)?;
    Ok(ClientSession { stream, negotiated })
}

fn hello_round_trip<S: Read + Write>(
    stream: &mut S,
    program: &str,
    version_hint: &str,
) -> Result<Negotiated, BrokerV2Error> {
    let hello = Hello {
        client_min_protocol: ENVELOPE_VERSION as u32,
        client_max_protocol: ENVELOPE_VERSION as u32,
        service_name: program.to_string(),
        wanted_version: version_hint.to_string(),
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        client_capabilities: 0,
        auth_token: Vec::new(),
        request_id: format!("client_v2-{program}-{}", std::process::id()),
        connection_id: 0,
        peer_pid: std::process::id(),
        client_lib_name: "running-process broker::client_v2".to_string(),
        client_lib_version: env!("CARGO_PKG_VERSION").to_string(),
        peer_attestation_nonce: Vec::new(),
        capability_token: Vec::new(),
        client_keepalive_secs: 0,
    };
    let mut body = Vec::with_capacity(hello.encoded_len());
    hello.encode(&mut body)?;
    write_frame(stream, &body)?;

    let reply_bytes = read_frame(stream)?;
    let reply = HelloReply::decode(reply_bytes.as_slice())?;
    match reply.result {
        Some(hello_reply::Result::Negotiated(n)) => Ok(n),
        Some(hello_reply::Result::Refused(r)) => Err(BrokerV2Error::Refused {
            reason: r.reason.clone(),
            retry_after_ms: r.retry_after_ms,
            details: Box::new(r),
        }),
        None => Err(BrokerV2Error::MissingResult),
    }
}

fn resolve_socket_path(bare_name: &str) -> String {
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\{bare_name}")
    }
    #[cfg(unix)]
    {
        use std::path::PathBuf;
        let dir: PathBuf = {
            #[cfg(target_os = "macos")]
            {
                let uid = unsafe { libc::getuid() };
                let tmp = std::env::var_os("TMPDIR")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("/tmp"));
                tmp.join(format!(".rp-{uid}-broker-v2"))
            }
            #[cfg(not(target_os = "macos"))]
            {
                if let Some(d) = std::env::var_os("XDG_RUNTIME_DIR") {
                    PathBuf::from(d).join("running-process").join("broker-v2")
                } else {
                    let uid = unsafe { libc::getuid() };
                    PathBuf::from(format!("/tmp/running-process-{uid}/broker-v2"))
                }
            }
        };
        let leaf = if cfg!(target_os = "macos") {
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
        dir.join(leaf).to_string_lossy().into_owned()
    }
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
mod tests {
    use super::*;
    use interprocess::local_socket::traits::Listener as _;
    use interprocess::local_socket::ListenerOptions;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    /// RAII guard: on `Drop`, removes the socket file at `path`. Used by
    /// [`spawn_stub_broker`] so a panic between bind and the final
    /// explicit `remove_file` doesn't leak a stale `.sock` that would
    /// poison the next test run.
    ///
    /// Fixes #519: previously, any panic between `tx.send` and the
    /// explicit `remove_file` left a stale socket. The next test run
    /// either got `EADDRINUSE` on bind or `ECONNREFUSED` on connect to
    /// the dead socket — both masking the real failure.
    #[cfg(unix)]
    struct SocketCleanup(std::path::PathBuf);

    #[cfg(unix)]
    impl Drop for SocketCleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// In-process stub broker: listens on the given path, accepts ONE
    /// connection, reads a Hello, sends back a `Negotiated` with
    /// `connection_id = 0xC0FFEE`. Returns nothing — the test asserts
    /// against the ClientSession the real client builds.
    fn spawn_stub_broker(socket_path: String) -> mpsc::Receiver<()> {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let name = wrap_socket_name(&socket_path).expect("wrap_socket_name");
            #[cfg(unix)]
            let _cleanup = {
                let _ =
                    std::fs::create_dir_all(std::path::Path::new(&socket_path).parent().unwrap());
                let _ = std::fs::remove_file(&socket_path);
                SocketCleanup(std::path::PathBuf::from(&socket_path))
            };
            let listener = ListenerOptions::new()
                .name(name)
                .create_sync()
                .expect("ListenerOptions create_sync");
            tx.send(()).expect("send listener-ready signal");
            let mut stream = listener.accept().expect("accept");
            let bytes = read_frame(&mut stream).expect("read Hello frame");
            let hello = Hello::decode(bytes.as_slice()).expect("decode Hello");
            let reply = HelloReply {
                result: Some(hello_reply::Result::Negotiated(Negotiated {
                    negotiated_protocol: ENVELOPE_VERSION as u32,
                    daemon_version: "stub-1.2.3".to_string(),
                    backend_pipe: String::new(),
                    warnings: Vec::new(),
                    server_capabilities: 0,
                    keepalive_interval_secs: 0,
                    handle_passed_token: Vec::new(),
                    connection_id: 0x00C0_FFEE,
                })),
            };
            let mut body = Vec::with_capacity(reply.encoded_len());
            reply.encode(&mut body).expect("encode HelloReply");
            write_frame(&mut stream, &body).expect("write HelloReply frame");
            // RAII guard removes the socket on scope exit; the explicit
            // remove that lived here previously was a no-op leftover.
            let _ = hello.service_name;
        });
        rx
    }

    #[test]
    fn connect_completes_hello_round_trip_against_stub_broker() {
        // Use a per-test program name so parallel tests don't collide.
        let program = "client-v2-stub";
        let sid = user_sid_hash().expect("user_sid_hash");
        let pipe_name = v2_program_pipe(program, &sid, 0).expect("pipe name");
        let socket_path = resolve_socket_path(&pipe_name);

        let ready = spawn_stub_broker(socket_path.clone());
        ready
            .recv_timeout(Duration::from_secs(2))
            .expect("stub broker listening");

        // The Listener on Windows is fully ready as soon as `create_sync`
        // returns; on Unix the same holds. But a short retry loop is
        // resilient to spawning race in CI.
        let start = Instant::now();
        let session = loop {
            match connect(program, "0.0.0") {
                Ok(s) => break s,
                Err(err) if start.elapsed() < Duration::from_secs(2) => {
                    eprintln!("connect retry after error: {err}");
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
                Err(err) => panic!("connect failed after retries: {err}"),
            }
        };

        let neg = session.negotiated();
        assert_eq!(neg.negotiated_protocol, ENVELOPE_VERSION as u32);
        assert_eq!(neg.connection_id, 0x00C0_FFEE);
        assert_eq!(neg.daemon_version, "stub-1.2.3");
    }

    #[test]
    fn connect_with_no_broker_returns_dial_error() {
        let err =
            connect("client-v2-no-broker-ever", "0.0.0").expect_err("no broker => Dial error");
        match err {
            BrokerV2Error::Dial { .. } => {}
            other => panic!("expected Dial, got: {other:?}"),
        }
    }

    /// In-process stub that accepts the dial then sleeps forever — the
    /// pathological case that motivated #517. Without the helper-thread
    /// deadline, the client hangs indefinitely.
    fn spawn_stall_broker(socket_path: String) -> mpsc::Receiver<()> {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let name = wrap_socket_name(&socket_path).expect("wrap_socket_name");
            #[cfg(unix)]
            let _cleanup = {
                let _ =
                    std::fs::create_dir_all(std::path::Path::new(&socket_path).parent().unwrap());
                let _ = std::fs::remove_file(&socket_path);
                SocketCleanup(std::path::PathBuf::from(&socket_path))
            };
            let listener = ListenerOptions::new()
                .name(name)
                .create_sync()
                .expect("ListenerOptions create_sync");
            tx.send(()).expect("send listener-ready signal");
            let _stream = listener.accept().expect("accept");
            // Stall — never reads the Hello, never replies. The deadline
            // bound on the client side is what releases it.
            thread::sleep(Duration::from_secs(60));
        });
        rx
    }

    /// `connect_with_deadline` returns `TimedOut` when the broker
    /// accepts then stalls. Fixes #517.
    #[test]
    fn connect_with_deadline_fires_on_stalling_broker() {
        let program = "client-v2-stall-deadline";
        let sid = user_sid_hash().expect("user_sid_hash");
        let pipe_name = v2_program_pipe(program, &sid, 0).expect("pipe name");
        let socket_path = resolve_socket_path(&pipe_name);
        let ready = spawn_stall_broker(socket_path);
        ready
            .recv_timeout(Duration::from_secs(2))
            .expect("stall broker listening");
        let start = Instant::now();
        let err = connect_with_deadline(program, "0.0.0", Duration::from_millis(200))
            .expect_err("stall broker => deadline TimedOut");
        let elapsed = start.elapsed();
        match err {
            BrokerV2Error::Io(io) => assert_eq!(io.kind(), std::io::ErrorKind::TimedOut),
            other => panic!("expected Io(TimedOut), got: {other:?}"),
        }
        assert!(
            elapsed < Duration::from_secs(2),
            "deadline should fire within budget; took {elapsed:?}"
        );
    }

    /// `BrokerV2Error::Refused` exposes `retry_after_ms` as a top-level
    /// field, mirroring v1's `BrokerClientError::Refused`. Fixes #518.
    /// Constructs a stub broker that replies with Refused, asserts the
    /// retry hint surfaces top-level (not buried in `details`).
    fn spawn_refusing_broker(socket_path: String, retry_after_ms: u64) -> mpsc::Receiver<()> {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let name = wrap_socket_name(&socket_path).expect("wrap_socket_name");
            #[cfg(unix)]
            let _cleanup = {
                let _ =
                    std::fs::create_dir_all(std::path::Path::new(&socket_path).parent().unwrap());
                let _ = std::fs::remove_file(&socket_path);
                SocketCleanup(std::path::PathBuf::from(&socket_path))
            };
            let listener = ListenerOptions::new()
                .name(name)
                .create_sync()
                .expect("ListenerOptions create_sync");
            tx.send(()).expect("send listener-ready signal");
            let mut stream = listener.accept().expect("accept");
            let _bytes = read_frame(&mut stream).expect("read Hello frame");
            let reply = HelloReply {
                result: Some(hello_reply::Result::Refused(Refused {
                    code: 0,
                    reason: "stub refusal".to_string(),
                    retry_after_ms,
                    ..Refused::default()
                })),
            };
            let mut body = Vec::with_capacity(reply.encoded_len());
            reply.encode(&mut body).expect("encode HelloReply");
            write_frame(&mut stream, &body).expect("write HelloReply frame");
        });
        rx
    }

    /// Stress stub: accepts `count` connections in a loop, replying
    /// Negotiated to each. Used by the concurrent-connect stress test
    /// to prove the client side doesn't deadlock or leak handles when
    /// many threads dial simultaneously.
    fn spawn_multi_accept_stub_broker(socket_path: String, count: usize) -> mpsc::Receiver<()> {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let name = wrap_socket_name(&socket_path).expect("wrap_socket_name");
            #[cfg(unix)]
            let _cleanup = {
                let _ =
                    std::fs::create_dir_all(std::path::Path::new(&socket_path).parent().unwrap());
                let _ = std::fs::remove_file(&socket_path);
                SocketCleanup(std::path::PathBuf::from(&socket_path))
            };
            let listener = ListenerOptions::new()
                .name(name)
                .create_sync()
                .expect("ListenerOptions create_sync");
            tx.send(()).expect("send listener-ready signal");
            for _ in 0..count {
                let mut stream = match listener.accept() {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let _ = read_frame(&mut stream).expect("read Hello frame");
                let reply = HelloReply {
                    result: Some(hello_reply::Result::Negotiated(Negotiated {
                        negotiated_protocol: ENVELOPE_VERSION as u32,
                        daemon_version: "stub-multi-1".to_string(),
                        backend_pipe: String::new(),
                        warnings: Vec::new(),
                        server_capabilities: 0,
                        keepalive_interval_secs: 0,
                        handle_passed_token: Vec::new(),
                        connection_id: 0x0FFF_F1EE,
                    })),
                };
                let mut body = Vec::with_capacity(reply.encoded_len());
                reply.encode(&mut body).expect("encode HelloReply");
                write_frame(&mut stream, &body).expect("write HelloReply frame");
            }
        });
        rx
    }

    /// Stress test: 8 concurrent `connect_with_deadline` calls against a
    /// multi-accept stub broker. All must succeed within wall-clock
    /// budget — the helper-thread + `recv_timeout` pattern must scale
    /// to concurrent callers without serializing on a global mutex or
    /// deadlocking on the channel.
    #[test]
    fn concurrent_connects_against_multi_accept_broker() {
        let program = "client-v2-concurrent-multi";
        let sid = user_sid_hash().expect("user_sid_hash");
        let pipe_name = v2_program_pipe(program, &sid, 0).expect("pipe name");
        let socket_path = resolve_socket_path(&pipe_name);
        const N: usize = 8;
        let ready = spawn_multi_accept_stub_broker(socket_path, N);
        ready
            .recv_timeout(Duration::from_secs(2))
            .expect("multi-accept broker listening");

        let start = Instant::now();
        let handles: Vec<_> = (0..N)
            .map(|_| {
                let p = program.to_string();
                thread::spawn(move || connect_with_deadline(&p, "0.0.0", Duration::from_secs(2)))
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let elapsed = start.elapsed();

        let ok = results.iter().filter(|r| r.is_ok()).count();
        assert_eq!(
            ok, N,
            "all {N} concurrent connects must succeed; got {ok} ok, full results: {results:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "concurrent connect took {elapsed:?}; expected < 5s"
        );
        for session in results.iter().flatten() {
            assert_eq!(session.negotiated().connection_id, 0x0FFF_F1EE);
            assert_eq!(session.negotiated().daemon_version, "stub-multi-1");
        }
    }

    /// Adversarial stub: accepts, reads Hello, replies with a HelloReply
    /// whose `result` oneof is `None` (proto3 default — easy bug if a
    /// future broker forgets to set the variant). Must surface as
    /// `BrokerV2Error::MissingResult`, not be mis-routed as success.
    fn spawn_missing_result_broker(socket_path: String) -> mpsc::Receiver<()> {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let name = wrap_socket_name(&socket_path).expect("wrap_socket_name");
            #[cfg(unix)]
            let _cleanup = {
                let _ =
                    std::fs::create_dir_all(std::path::Path::new(&socket_path).parent().unwrap());
                let _ = std::fs::remove_file(&socket_path);
                SocketCleanup(std::path::PathBuf::from(&socket_path))
            };
            let listener = ListenerOptions::new()
                .name(name)
                .create_sync()
                .expect("ListenerOptions create_sync");
            tx.send(()).expect("send listener-ready signal");
            let mut stream = listener.accept().expect("accept");
            let _ = read_frame(&mut stream).expect("read Hello frame");
            let reply = HelloReply { result: None };
            let mut body = Vec::with_capacity(reply.encoded_len());
            reply.encode(&mut body).expect("encode HelloReply");
            write_frame(&mut stream, &body).expect("write HelloReply frame");
        });
        rx
    }

    #[test]
    fn connect_rejects_hello_reply_with_missing_result_oneof() {
        let program = "client-v2-missing-result";
        let sid = user_sid_hash().expect("user_sid_hash");
        let pipe_name = v2_program_pipe(program, &sid, 0).expect("pipe name");
        let socket_path = resolve_socket_path(&pipe_name);
        let ready = spawn_missing_result_broker(socket_path);
        ready
            .recv_timeout(Duration::from_secs(2))
            .expect("missing-result broker listening");
        let start = Instant::now();
        let err = loop {
            match connect(program, "0.0.0") {
                Err(e) => break e,
                Ok(_) if start.elapsed() < Duration::from_secs(2) => {
                    thread::sleep(Duration::from_millis(50));
                    continue;
                }
                Ok(_) => panic!("expected MissingResult, got Ok"),
            }
        };
        assert!(
            matches!(err, BrokerV2Error::MissingResult),
            "expected MissingResult, got: {err:?}"
        );
    }

    /// Adversarial: broker accepts then immediately drops the stream
    /// without reading the Hello or writing a reply. Must surface as
    /// a typed transport error (Framing/Io), never as a successful
    /// session, never hang past the deadline.
    fn spawn_drop_on_accept_broker(socket_path: String) -> mpsc::Receiver<()> {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let name = wrap_socket_name(&socket_path).expect("wrap_socket_name");
            #[cfg(unix)]
            let _cleanup = {
                let _ =
                    std::fs::create_dir_all(std::path::Path::new(&socket_path).parent().unwrap());
                let _ = std::fs::remove_file(&socket_path);
                SocketCleanup(std::path::PathBuf::from(&socket_path))
            };
            let listener = ListenerOptions::new()
                .name(name)
                .create_sync()
                .expect("ListenerOptions create_sync");
            tx.send(()).expect("send listener-ready signal");
            let stream = listener.accept().expect("accept");
            drop(stream); // immediate close
        });
        rx
    }

    #[test]
    fn connect_returns_err_on_premature_disconnect() {
        let program = "client-v2-prem-disconnect";
        let sid = user_sid_hash().expect("user_sid_hash");
        let pipe_name = v2_program_pipe(program, &sid, 0).expect("pipe name");
        let socket_path = resolve_socket_path(&pipe_name);
        let ready = spawn_drop_on_accept_broker(socket_path);
        ready
            .recv_timeout(Duration::from_secs(2))
            .expect("drop-on-accept broker listening");
        let start = Instant::now();
        let err = loop {
            match connect_with_deadline(program, "0.0.0", Duration::from_millis(500)) {
                Err(e) => break e,
                Ok(_) if start.elapsed() < Duration::from_secs(2) => {
                    thread::sleep(Duration::from_millis(50));
                    continue;
                }
                Ok(_) => panic!("expected transport error, got Ok"),
            }
        };
        // The exact variant depends on whether the write or read hits the
        // disconnect first: Framing(UnexpectedEof), Io(BrokenPipe), or
        // Dial (rare race). All are transport-class — none is a session.
        match err {
            BrokerV2Error::Framing(_) | BrokerV2Error::Io(_) | BrokerV2Error::Dial { .. } => {}
            other => panic!("expected transport variant, got: {other:?}"),
        }
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "must not hang past deadline; took {:?}",
            start.elapsed()
        );
    }

    /// Adversarial: every malformed program name must be rejected BEFORE
    /// `Stream::connect` runs — proves `v2_program_pipe`'s validation is
    /// the front gate. Catches NUL injection, path traversal, uppercase,
    /// over-long names, and empties. The expected error variant is
    /// `BrokerV2Error::PipeName(_)` because `v2_program_pipe`'s
    /// `validate_service_name` fires before any IO.
    #[test]
    fn connect_rejects_invalid_program_names_before_dial() {
        let too_long = "a".repeat(65);
        for bad in [
            "zccache\0evil",
            "../etc/passwd",
            r"a\b",
            "Zccache",
            "a b",
            too_long.as_str(),
            "",
        ] {
            let err = connect(bad, "0.0.0")
                .expect_err(&format!("invalid program name {bad:?} must be rejected"));
            assert!(
                matches!(err, BrokerV2Error::PipeName(_)),
                "expected PipeName for {bad:?}, got: {err:?}"
            );
        }
    }

    /// Pin u64::MAX round-trips through `retry_after_ms` without overflow.
    /// `Duration::from_millis(u64::MAX)` is valid (~584M years); locks
    /// the contract for any caller doing `Duration::from_millis(retry_after_ms)`.
    #[test]
    fn refused_with_u64_max_retry_after_ms_round_trips() {
        let program = "client-v2-refused-u64-max";
        let sid = user_sid_hash().expect("user_sid_hash");
        let pipe_name = v2_program_pipe(program, &sid, 0).expect("pipe name");
        let socket_path = resolve_socket_path(&pipe_name);
        let ready = spawn_refusing_broker(socket_path, u64::MAX);
        ready
            .recv_timeout(Duration::from_secs(2))
            .expect("refusing broker listening");
        let start = Instant::now();
        let err = loop {
            match connect(program, "0.0.0") {
                Err(e) => break e,
                Ok(_) if start.elapsed() < Duration::from_secs(2) => {
                    thread::sleep(Duration::from_millis(50));
                    continue;
                }
                Ok(_) => panic!("expected Refused, got Ok"),
            }
        };
        match err {
            BrokerV2Error::Refused {
                retry_after_ms,
                details,
                ..
            } => {
                assert_eq!(retry_after_ms, u64::MAX);
                assert_eq!(details.retry_after_ms, u64::MAX);
                // Caller-side contract: this Duration construction must not panic.
                let _safe_duration = Duration::from_millis(retry_after_ms);
            }
            other => panic!("expected Refused, got: {other:?}"),
        }
    }

    #[test]
    fn refused_exposes_retry_after_ms_top_level() {
        let program = "client-v2-refused-retry";
        let sid = user_sid_hash().expect("user_sid_hash");
        let pipe_name = v2_program_pipe(program, &sid, 0).expect("pipe name");
        let socket_path = resolve_socket_path(&pipe_name);
        let ready = spawn_refusing_broker(socket_path, 1234);
        ready
            .recv_timeout(Duration::from_secs(2))
            .expect("refusing broker listening");
        let start = Instant::now();
        let err = loop {
            match connect(program, "0.0.0") {
                Err(e) => break e,
                Ok(_) if start.elapsed() < Duration::from_secs(2) => {
                    thread::sleep(Duration::from_millis(50));
                    continue;
                }
                Ok(_) => panic!("expected Refused"),
            }
        };
        match err {
            BrokerV2Error::Refused {
                retry_after_ms,
                reason,
                details,
            } => {
                assert_eq!(
                    retry_after_ms, 1234,
                    "retry hint must surface top-level (was: {retry_after_ms})"
                );
                assert_eq!(reason, "stub refusal");
                assert_eq!(
                    details.retry_after_ms, 1234,
                    "details payload still carries the field for full diagnostics"
                );
            }
            other => panic!("expected Refused, got: {other:?}"),
        }
    }
}
