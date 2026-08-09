//! SESSION `0x5350` transport anchor (soldr#2388 Step 7 / #2386 Option A,
//! topology (c)): a compile session proxied **client → broker relay → endpoint**
//! over real local sockets.
//!
//! The daemon's *real* codec-bridge endpoint driving *real* rustc is already
//! proven by soldr-daemon's `session_endpoint` tests (Step 6d). This test proves
//! the piece 6d does not: soldr's client (`run_session_compile_with`) dialing
//! soldr's broker companion relay (`spawn_session_relay`, the proven async
//! `serve_broker_session_socket`) and having its `SessionStart` full-proxied to,
//! and its stdout/exit streamed back from, the endpoint at `backend_pipe`. A
//! deterministic mock endpoint stands in for the daemon so the transport is
//! isolated from a real compile.

use std::time::Duration;

use running_process::broker::protocol_v2::{
    session_frame, SessionEnvVar, SessionExit, SessionFrame,
};
use running_process::broker::session_codec::{encode_session_frame, try_decode_session_frame};
use soldr_cli::daemon::session_endpoint::bind_session_listener;
use soldr_cli::session_transport::{run_session_compile_with, spawn_session_relay};

/// A unique endpoint path for this test: a temp socket file on Unix, a
/// namespaced pipe name on Windows.
fn unique_endpoint_path() -> (String, Option<tempfile::TempDir>) {
    #[cfg(unix)]
    {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mock-session.sock");
        (path.display().to_string(), Some(dir))
    }
    #[cfg(windows)]
    {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        (
            format!(
                "soldr-session-e2e-endpoint-{}-{}",
                std::process::id(),
                nanos
            ),
            None,
        )
    }
}

/// Serve one SESSION connection as a deterministic daemon stand-in: read frames
/// until the opening `SessionStart`, then emit a stdout chunk and a terminal
/// `Exit{code}`.
async fn mock_endpoint_respond<S>(mut conn: S, code: i32) -> std::io::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut saw_start = false;
    while !saw_start {
        while let Some(decoded) = try_decode_session_frame(&buf).expect("decode session frame") {
            let consumed = decoded.consumed;
            let is_start = matches!(decoded.frame.kind, Some(session_frame::Kind::Start(_)));
            buf.drain(..consumed);
            if is_start {
                saw_start = true;
                break;
            }
        }
        if saw_start {
            break;
        }
        let n = conn.read(&mut chunk).await?;
        if n == 0 {
            return Err(std::io::Error::other("endpoint closed before SessionStart"));
        }
        buf.extend_from_slice(&chunk[..n]);
    }

    let stdout = encode_session_frame(
        &SessionFrame {
            kind: Some(session_frame::Kind::Stdout(
                b"mock endpoint stdout\n".to_vec(),
            )),
        },
        0,
    )
    .expect("encode stdout");
    conn.write_all(&stdout).await?;
    let exit = encode_session_frame(
        &SessionFrame {
            kind: Some(session_frame::Kind::Exit(SessionExit {
                code,
                ..Default::default()
            })),
        },
        1,
    )
    .expect("encode exit");
    conn.write_all(&exit).await?;
    conn.flush().await?;
    Ok(())
}

soldr_cli::timed_test!(
    session_relay_proxies_client_to_endpoint,
    Duration::from_secs(60),
    {
        use interprocess::local_socket::tokio::prelude::*;

        const EXIT_CODE: i32 = 7;
        let program = format!("soldr-session-e2e-{}", std::process::id());
        let (endpoint_path, _guard) = unique_endpoint_path();

        // 1) Mock daemon SESSION endpoint on its own thread + runtime. Bind
        //    synchronously (so the relay's dial always finds it), then accept one
        //    connection and respond.
        let endpoint_for_thread = endpoint_path.clone();
        let endpoint_thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("endpoint runtime");
            rt.block_on(async move {
                let listener =
                    bind_session_listener(&endpoint_for_thread).expect("bind mock endpoint");
                let conn = listener.accept().await.expect("accept at endpoint");
                mock_endpoint_respond(conn, EXIT_CODE)
                    .await
                    .expect("endpoint respond");
            });
        });

        // 2) Broker companion SESSION relay → the mock endpoint.
        spawn_session_relay(&program, endpoint_path.clone()).expect("spawn session relay");

        // 3) Client drives the compile over the relay. The relay thread binds its
        //    socket asynchronously, so retry the dial until it is up.
        let argv = vec!["rustc".to_string(), "--version".to_string()];
        let env: Vec<SessionEnvVar> = Vec::new();
        let mut exit = None;
        for attempt in 0..40 {
            match run_session_compile_with(&program, &argv, ".".to_string(), env.clone()) {
                Ok(code) => {
                    exit = Some(code);
                    break;
                }
                Err(err) => {
                    assert!(
                        attempt < 39,
                        "client never reached the relay after retries: {err}"
                    );
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }

        assert_eq!(
            exit,
            Some(EXIT_CODE),
            "exit code proxied client -> broker relay -> endpoint"
        );
        endpoint_thread.join().expect("endpoint thread");
    }
);
