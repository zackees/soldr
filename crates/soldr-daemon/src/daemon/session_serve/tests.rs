//! End-to-end test for the SESSION codec-bridge (soldr#2388 Step 6c): a **real
//! rustc compile** driven client → `serve_session_compile` → embedded zccache →
//! `SessionFrame`s, over an in-memory duplex. The executable form of fable5's
//! answer-A ruling: the SESSION compile flows through the embedded service and
//! its output comes back as SessionFrames carrying `cache_outcome`/`compile_id`
//! — with **no spawned child at the session layer** (zccache owns any rustc
//! child internally).

use running_process::broker::backend_handle::DaemonProcess;
use running_process::broker::protocol::Endpoint;
use running_process::broker::protocol_v2::{
    session_frame, SessionEnvVar, SessionExit, SessionFrame,
};
use running_process::broker::session_codec::{encode_session_frame, try_decode_session_frame};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::serve_session_compile;
use crate::core::SoldrPaths;
use crate::zccache_embedded::SoldrZccacheService;

fn test_daemon_identity() -> DaemonProcess {
    let endpoint = Endpoint {
        namespace_id: "shared".into(),
        path: "rpb-session-e2e".into(),
    };
    DaemonProcess::current_process(endpoint, Some(30)).expect("current-process identity")
}

/// Drain every `SessionFrame` off `client` until EOF, reporting whether a
/// terminal `Exit` frame ever arrived. Used by the disconnect test: a compile
/// aborted for client-disconnect must NOT ship an `Exit` (the wrapper is gone).
async fn drain_frames_saw_exit<R>(client: &mut R) -> bool
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut saw_exit = false;
    loop {
        while let Some(decoded) = try_decode_session_frame(&buf).expect("decode session frame") {
            let consumed = decoded.consumed;
            if matches!(decoded.frame.kind, Some(session_frame::Kind::Exit(_))) {
                saw_exit = true;
            }
            buf.drain(..consumed);
        }
        let n = client.read(&mut chunk).await.expect("read from bridge");
        if n == 0 {
            return saw_exit;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Read `SessionFrame`s off `client` until the terminal `Exit`, returning the
/// accumulated stdout and the exit frame.
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
        let n = client.read(&mut chunk).await.expect("read from bridge");
        assert!(n > 0, "bridge closed before sending Exit");
        buf.extend_from_slice(&chunk[..n]);
    }
}

#[test]
fn session_compile_e2e_real_rustc_through_the_bridge() {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            // Real rustc + the repo's pinned toolchain, mirroring the
            // embedded-service compile test in `zccache_embedded`.
            let current_dir = std::env::current_dir().expect("cwd");
            let repo = current_dir
                .ancestors()
                .find(|c| c.join("rust-toolchain.toml").is_file())
                .expect("find repo rust-toolchain.toml");
            let pinned = crate::core::read_rust_toolchain_manifest(repo)
                .expect("read rust-toolchain.toml")
                .channel
                .expect("rust-toolchain.toml declares a channel");
            let rustc = crate::test_support::rustc_from_env_or_path();

            let temp = tempfile::tempdir().expect("tempdir");
            let project = temp.path().join("workspace");
            std::fs::create_dir_all(project.join("src")).expect("create src dir");
            std::fs::write(
                project.join("src/lib.rs"),
                "pub fn session_answer() -> u32 { 1651 }\n",
            )
            .expect("write source");

            // SessionStart carries the command the client would have exec'd:
            // program = rustc path, args = the rustc arguments.
            let args: Vec<String> = vec![
                "--edition".into(),
                "2021".into(),
                "--crate-type".into(),
                "lib".into(),
                "--crate-name".into(),
                "soldr_session_e2e".into(),
                "--emit=metadata".into(),
                "-C".into(),
                "metadata=ses1".into(),
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
            let start = super::SessionStart {
                program: rustc.display().to_string(),
                args,
                cwd: project.display().to_string(),
                env,
                clear_inherited_env: true,
                environment_policy: running_process::broker::protocol_v2::EnvironmentPolicy::Clear
                    as i32,
            };

            // Real embedded zccache service in an isolated temp root.
            let daemon = test_daemon_identity();
            let paths = SoldrPaths::with_root(temp.path().join("root"));
            let service = SoldrZccacheService::start(&paths, &daemon)
                .await
                .expect("start embedded zccache service");

            // Drive the bridge and the client concurrently over one duplex
            // (join!, not spawn: the bridge borrows &service/&paths).
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

            let (bridge_res, (stdout, exit)) =
                tokio::join!(serve_session_compile(server, &service, &paths), client_fut);
            bridge_res.expect("bridge serve ok");

            assert_eq!(
                exit.code,
                0,
                "real rustc compile must succeed over SESSION; stdout={}",
                String::from_utf8_lossy(&stdout)
            );
            assert!(
                exit.metadata
                    .contains_key(crate::daemon::session_sink::META_CACHE_OUTCOME),
                "SessionExit.metadata carries cache_outcome (no observability regression)"
            );
            assert!(
                exit.metadata
                    .contains_key(crate::daemon::session_sink::META_COMPILE_ID),
                "SessionExit.metadata carries compile_id"
            );
        });
}

#[test]
fn session_client_disconnect_mid_compile_aborts_without_reply() {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let current_dir = std::env::current_dir().expect("cwd");
            let repo = current_dir
                .ancestors()
                .find(|c| c.join("rust-toolchain.toml").is_file())
                .expect("find repo rust-toolchain.toml");
            let pinned = crate::core::read_rust_toolchain_manifest(repo)
                .expect("read rust-toolchain.toml")
                .channel
                .expect("rust-toolchain.toml declares a channel");
            let rustc = crate::test_support::rustc_from_env_or_path();

            // A fresh isolated root guarantees a COLD compile — a real rustc
            // spawn, not an instant zccache hit — so the client-disconnect
            // deterministically wins the race against a still-pending compile.
            let temp = tempfile::tempdir().expect("tempdir");
            let project = temp.path().join("workspace");
            std::fs::create_dir_all(project.join("src")).expect("create src dir");
            std::fs::write(
                project.join("src/lib.rs"),
                "pub fn disc() -> u32 { 4242 }\n",
            )
            .expect("write source");

            let args: Vec<String> = vec![
                "--edition".into(),
                "2021".into(),
                "--crate-type".into(),
                "lib".into(),
                "--crate-name".into(),
                "soldr_session_disc".into(),
                "--emit=metadata".into(),
                "-C".into(),
                "metadata=disc1".into(),
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
            let start = super::SessionStart {
                program: rustc.display().to_string(),
                args,
                cwd: project.display().to_string(),
                env,
                clear_inherited_env: true,
                environment_policy: running_process::broker::protocol_v2::EnvironmentPolicy::Clear
                    as i32,
            };

            let daemon = test_daemon_identity();
            let paths = SoldrPaths::with_root(temp.path().join("root"));
            let service = SoldrZccacheService::start(&paths, &daemon)
                .await
                .expect("start embedded zccache service");

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
                // Close the write half before the compile can finish. The
                // daemon's disconnect probe (biased `select!`, reader first)
                // reads EOF while the zccache compile future is still pending
                // on the rustc spawn, so the disconnect deterministically
                // wins. The read half stays open so we can prove the daemon
                // shipped NO reply.
                client.shutdown().await.expect("shutdown write half");
                drain_frames_saw_exit(&mut client).await
            };

            let (bridge_res, saw_exit) =
                tokio::join!(serve_session_compile(server, &service, &paths), client_fut);
            bridge_res.expect("bridge serve returns Ok even on client disconnect");
            assert!(
                !saw_exit,
                "a client that disconnected mid-compile must not receive an Exit frame — \
                     the SESSION path must cancel the in-flight compile, not run it to completion",
            );
        });
}
