//! End-to-end tests for the SESSION endpoint per-connection handler
//! (soldr#2388 Step 6d). Both Option-A properties are proven on the **same**
//! handler over an in-memory duplex:
//!
//! - `session_endpoint_serves_real_compile_via_mux`: a real rustc compile flows
//!   client → `serve_session_connection` (mux `Payload{0x5350}` → replay →
//!   `serve_session_compile`) → embedded zccache → `SessionFrame`s, exit 0 with
//!   `cache_outcome`/`compile_id` metadata. No spawned child at the session
//!   layer.
//! - `session_endpoint_answers_backend_handle_probe`: a `BackendHandle` liveness
//!   probe on the same handler is answered (mux `ProbeAnswered`), so the SESSION
//!   endpoint stays broker-probe-compatible — the coexistence Option A requires.

use std::time::Duration;

use running_process::broker::backend_handle::DaemonProcess;
use running_process::broker::protocol::{
    encode_framed, try_decode_framed, Endpoint, Frame, BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL,
};
use running_process::broker::protocol_v2::{
    session_frame, SessionEnvVar, SessionExit, SessionFrame,
};
use running_process::broker::session_codec::{encode_session_frame, try_decode_session_frame};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use std::sync::Arc;

use super::{
    bind_session_listener, local_session_name, serve_session_connection, serve_session_endpoint,
    soldr_session_endpoint_mux,
};
use crate::core::SoldrPaths;
use crate::zccache_embedded::SoldrZccacheService;

/// Nonce length for a `BackendHandle` endpoint probe request
/// (`endpoint_probe_request_from_frame` requires exactly 32 bytes).
const PROBE_NONCE_BYTES: usize = 32;

fn test_daemon_identity() -> DaemonProcess {
    let endpoint = Endpoint {
        namespace_id: "shared".into(),
        path: "rpb-session-endpoint-e2e".into(),
    };
    DaemonProcess::current_process(endpoint, Some(30)).expect("current-process identity")
}

/// Read `SessionFrame`s off `client` until the terminal `Exit`.
async fn read_until_exit<R>(client: &mut R) -> (Vec<u8>, SessionExit)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut stdout = Vec::new();
    loop {
        while let Some(decoded) = try_decode_session_frame(&buf).expect("decode session frame") {
            let consumed = decoded.consumed;
            let kind = decoded.frame.kind.clone();
            buf.drain(..consumed);
            match kind {
                Some(session_frame::Kind::Stdout(b)) => stdout.extend_from_slice(&b),
                Some(session_frame::Kind::Stderr(_)) => {}
                Some(session_frame::Kind::Exit(exit)) => return (stdout, exit),
                _ => {}
            }
        }
        let n = client.read(&mut chunk).await.expect("read from handler");
        assert!(n > 0, "handler closed before sending Exit");
        buf.extend_from_slice(&chunk[..n]);
    }
}

crate::timed_test!(
    session_endpoint_serves_real_compile_via_mux,
    Duration::from_secs(300),
    {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                // Real rustc + the repo's pinned toolchain (mirrors the 6c
                // bridge test), but driven through the endpoint's mux handler.
                let current_dir = std::env::current_dir().expect("cwd");
                let repo = current_dir
                    .ancestors()
                    .find(|c| c.join("rust-toolchain.toml").is_file())
                    .expect("find repo rust-toolchain.toml");
                let pinned = crate::core::read_rust_toolchain_manifest(repo)
                    .expect("read rust-toolchain.toml")
                    .channel
                    .expect("rust-toolchain.toml declares a channel");
                let rustc = zccache::test_support::find_rustc()
                    .expect("Rust compiler prerequisite failed: no rustc on PATH");

                let temp = tempfile::tempdir().expect("tempdir");
                let project = temp.path().join("workspace");
                std::fs::create_dir_all(project.join("src")).expect("create src dir");
                std::fs::write(
                    project.join("src/lib.rs"),
                    "pub fn endpoint_answer() -> u32 { 6060 }\n",
                )
                .expect("write source");

                let args: Vec<String> = vec![
                    "--edition".into(),
                    "2021".into(),
                    "--crate-type".into(),
                    "lib".into(),
                    "--crate-name".into(),
                    "soldr_session_endpoint_e2e".into(),
                    "--emit=metadata".into(),
                    "-C".into(),
                    "metadata=sep1".into(),
                    "--out-dir".into(),
                    "target/debug/deps".into(),
                    "src/lib.rs".into(),
                ];
                let mut env: Vec<SessionEnvVar> = std::env::vars()
                    .filter(|(k, _)| k != "RUSTUP_TOOLCHAIN")
                    .map(|(key, value)| SessionEnvVar { key, value })
                    .collect();
                env.push(SessionEnvVar {
                    key: "RUSTUP_TOOLCHAIN".into(),
                    value: pinned,
                });
                let start = running_process::broker::protocol_v2::SessionStart {
                    program: rustc.as_path().display().to_string(),
                    args,
                    cwd: project.display().to_string(),
                    env,
                    clear_inherited_env: false,
                };

                let daemon = test_daemon_identity();
                let paths = SoldrPaths::with_root(temp.path().join("root"));
                let service = SoldrZccacheService::start(&paths, &daemon)
                    .await
                    .expect("start embedded zccache service");
                let mux = soldr_session_endpoint_mux(daemon);

                let (mut client, server) = tokio::io::duplex(1 << 20);
                let client_fut = async {
                    let hello = encode_session_frame(
                        &SessionFrame {
                            kind: Some(session_frame::Kind::Start(start)),
                        },
                        0,
                    )
                    .expect("encode SessionStart");
                    client.write_all(&hello).await.expect("send SessionStart");
                    client.flush().await.expect("flush");
                    read_until_exit(&mut client).await
                };

                let (handler_res, (stdout, exit)) = tokio::join!(
                    serve_session_connection(server, &service, &paths, &mux),
                    client_fut
                );
                handler_res.expect("handler serve ok");

                assert_eq!(
                    exit.code,
                    0,
                    "real rustc compile must succeed through the endpoint handler; stdout={}",
                    String::from_utf8_lossy(&stdout)
                );
                assert!(
                    exit.metadata
                        .contains_key(crate::daemon::session_sink::META_CACHE_OUTCOME),
                    "SessionExit.metadata carries cache_outcome"
                );
                assert!(
                    exit.metadata
                        .contains_key(crate::daemon::session_sink::META_COMPILE_ID),
                    "SessionExit.metadata carries compile_id"
                );
            });
    }
);

crate::timed_test!(session_endpoint_answers_backend_handle_probe, {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let daemon = test_daemon_identity();
            let mux = soldr_session_endpoint_mux(daemon);

            // The compile service is not touched on the probe branch, but the
            // handler signature requires one; an isolated embedded service is
            // the honest way to prove probe + SESSION coexist on one handler.
            let temp = tempfile::tempdir().expect("tempdir");
            let paths = SoldrPaths::with_root(temp.path().join("root"));
            let service = SoldrZccacheService::start(&paths, &test_daemon_identity())
                .await
                .expect("start embedded zccache service");

            // Build a BackendHandle endpoint probe request: Frame{0xB232, nonce}.
            let nonce = vec![7u8; PROBE_NONCE_BYTES];
            let request_id = 42;
            let probe = Frame::request(BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL, nonce)
                .with_request_id(request_id);
            let probe_bytes = encode_framed(&probe).expect("encode probe request");

            let (mut client, server) = tokio::io::duplex(1 << 16);
            let client_fut = async {
                client.write_all(&probe_bytes).await.expect("send probe");
                client.flush().await.expect("flush");
                // Read the framed reply, then hang up so the handler's next read
                // hits EOF and it returns cleanly.
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let reply = loop {
                    if let Some(decoded) =
                        try_decode_framed(&buf).expect("decode framed probe reply")
                    {
                        break decoded.frame;
                    }
                    let n = client.read(&mut chunk).await.expect("read probe reply");
                    assert!(n > 0, "handler closed before answering probe");
                    buf.extend_from_slice(&chunk[..n]);
                };
                client.shutdown().await.ok();
                reply
            };

            let (handler_res, reply) = tokio::join!(
                serve_session_connection(server, &service, &paths, &mux),
                client_fut
            );
            handler_res.expect("handler serve ok");

            assert_eq!(
                reply.payload_protocol, BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL,
                "reply is a BackendHandle probe response"
            );
            assert_eq!(
                reply.request_id, request_id,
                "probe response echoes the request id"
            );
        });
});

/// A unique local-socket name for a test: a filesystem path under `temp` on
/// Unix, a namespaced pipe name on Windows.
fn unique_session_socket(temp: &tempfile::TempDir) -> String {
    #[cfg(unix)]
    {
        temp.path().join("session.sock").display().to_string()
    }
    #[cfg(windows)]
    {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let _ = temp;
        format!("soldr-session-6d-{}-{}", std::process::id(), nanos)
    }
}

crate::timed_test!(session_endpoint_accept_loop_binds_and_dispatches_probe, {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            use interprocess::local_socket::tokio::prelude::*;
            use interprocess::local_socket::tokio::Stream;

            let temp = tempfile::tempdir().expect("tempdir");
            let socket = unique_session_socket(&temp);
            let listener = bind_session_listener(&socket).expect("bind SESSION listener");

            let paths = SoldrPaths::with_root(temp.path().join("root"));
            let service = Arc::new(
                SoldrZccacheService::start(&paths, &test_daemon_identity())
                    .await
                    .expect("start embedded zccache service"),
            );
            let mux = Arc::new(soldr_session_endpoint_mux(test_daemon_identity()));
            let server = tokio::spawn(serve_session_endpoint(listener, service, paths, mux));

            // Dial the real transport (the listener is bound before accept runs,
            // so the connect queues in the backlog).
            let name = local_session_name(&socket).expect("socket name");
            let mut client = Stream::connect(name)
                .await
                .expect("connect SESSION endpoint");

            let nonce = vec![9u8; PROBE_NONCE_BYTES];
            let request_id = 77;
            let probe = Frame::request(BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL, nonce)
                .with_request_id(request_id);
            let probe_bytes = encode_framed(&probe).expect("encode probe request");
            client.write_all(&probe_bytes).await.expect("send probe");
            client.flush().await.expect("flush");

            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let reply = loop {
                if let Some(decoded) = try_decode_framed(&buf).expect("decode framed probe reply") {
                    break decoded.frame;
                }
                let n = client.read(&mut chunk).await.expect("read probe reply");
                assert!(n > 0, "endpoint closed before answering probe");
                buf.extend_from_slice(&chunk[..n]);
            };

            assert_eq!(
                reply.payload_protocol, BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL,
                "reply is a BackendHandle probe response"
            );
            assert_eq!(reply.request_id, request_id, "probe response echoes id");

            server.abort();
        });
});
