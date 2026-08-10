//! Adversarial integration tests for the daemon → subprocess stdout seam.
//!
//! The daemon spawns subprocesses via `running_process::spawn_daemon`,
//! which wires stdin/stdout/stderr to the platform null device. That's a
//! deliberate API choice — once the subprocess is detached from the
//! daemon's lifetime, there's no caller around to hold a pipe. These
//! tests pin the consequences of that choice from both directions:
//!
//! - **Negative**: there's no path for the client to read the subprocess's
//!   `println!` output via the daemon. A subprocess writing to stdout is
//!   writing to `/dev/null` / `NUL`.
//! - **Positive**: the client can hand a file path (or any other os-named
//!   resource) to the subprocess via env / argv, and the subprocess can
//!   use that as a side channel.
//! - **Robustness**: high-volume writes don't block, multiple subprocesses
//!   don't interfere, non-existent cwd is rejected up front rather than
//!   silently rewritten to the daemon's own cwd.
//!
//! Each test is deterministic — they probe OS-level behaviour, not race
//! conditions — so a failure here is a real bug, not a flake.

use running_process::daemon::client::{DaemonClient, SpawnCommandRequest};
use running_process::proto::daemon::StatusCode;

use super::{scaled, start_server_with_tempdb};

// ── 1. high-volume stdout doesn't block ─────────────────────────────────────

/// The subprocess loops printing chunks until it has emitted ~256 KiB.
/// Stdout is NUL, so writes never block on backpressure. The subprocess
/// must reach the explicit `done`-marker file (which we read back) rather
/// than hanging on stdout buffer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subprocess_stdout_high_volume_does_not_block() {
    let scope = format!("seam-stdout-vol-{}", line!());
    let (server_handle, socket, _tmp_dir) = start_server_with_tempdb(&scope);
    tokio::time::sleep(scaled(std::time::Duration::from_millis(300))).await;

    let workdir = tempfile::tempdir().expect("tempdir");
    let done = workdir.path().join("done.txt");
    let done_str = done.to_string_lossy().into_owned();

    // 256 chunks * ~1 KiB chunk = ~256 KiB of stdout into NUL. Then
    // write the marker file so we know the loop finished.
    let command = if cfg!(windows) {
        // cmd.exe: build a ~1 KiB string by repeating `x` 1024 times via
        // `setlocal enabledelayedexpansion` is overkill. Easier: write
        // many short echo lines.
        format!(
            "for /L %i in (1,1,256) do @echo \
xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\
xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\
xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\
xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\
xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\
xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\
xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\
xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\
xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\
xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\
xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\
xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\
xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\
xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\
xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\
xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx & echo done> \"{done_str}\""
        )
    } else {
        format!(
            "i=0; while [ $i -lt 256 ]; do \
             printf '%s\\n' $(yes x | head -c 1024 | tr -d '\\n'); \
             i=$((i+1)); done; echo done > \"{done_str}\""
        )
    };

    let socket_for_client = socket.clone();
    let done_for_client = done.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket_for_client).expect("connect");
        let spawned = client
            .spawn_command(&SpawnCommandRequest::shell(command))
            .expect("spawn_command");
        assert!(spawned.pid > 0);

        // Poll for the done marker — up to 10s, scaled on CI.
        let deadline = std::time::Instant::now() + scaled(std::time::Duration::from_secs(10));
        while !done_for_client.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            done_for_client.exists(),
            "subprocess never reached the done marker — stdout-to-NUL \
             must not be blocking the shell loop"
        );

        let _ = client.shutdown(true, 5.0);
    })
    .await;
    result.expect("client task");

    let _ = tokio::time::timeout(scaled(std::time::Duration::from_secs(5)), server_handle).await;
}

// ── 2. subprocess reading stdin gets EOF ────────────────────────────────────

/// Stdin is wired to NUL/`/dev/null` — any read returns EOF immediately.
/// We exercise this by spawning a subprocess that reads stdin, and
/// verifying it COMPLETES (rather than blocking forever waiting for
/// input). The marker file proves the read returned.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subprocess_stdin_returns_eof() {
    let scope = format!("seam-stdin-eof-{}", line!());
    let (server_handle, socket, _tmp_dir) = start_server_with_tempdb(&scope);
    tokio::time::sleep(scaled(std::time::Duration::from_millis(300))).await;

    let workdir = tempfile::tempdir().expect("tempdir");
    let done = workdir.path().join("read-completed.txt");
    let done_str = done.to_string_lossy().into_owned();

    let command = if cfg!(windows) {
        // `set /p var=` on cmd reads a line from stdin. With stdin
        // wired to NUL it returns immediately (EOF). Then we write
        // the marker.
        format!("set /p var= & echo done> \"{done_str}\"")
    } else {
        // `read` exits non-zero on EOF — we explicitly tolerate that
        // with `|| true` so the shell still reaches the marker write.
        format!("read line || true; printf done > \"{done_str}\"")
    };

    let socket_for_client = socket.clone();
    let done_for_client = done.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket_for_client).expect("connect");
        let _spawned = client
            .spawn_command(&SpawnCommandRequest::shell(command))
            .expect("spawn_command");

        let deadline = std::time::Instant::now() + scaled(std::time::Duration::from_secs(5));
        while !done_for_client.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            done_for_client.exists(),
            "subprocess hung reading stdin — should return EOF immediately"
        );

        let _ = client.shutdown(true, 5.0);
    })
    .await;
    result.expect("client task");

    let _ = tokio::time::timeout(scaled(std::time::Duration::from_secs(5)), server_handle).await;
}

// ── 3. concurrent subprocesses write distinct files ─────────────────────────

/// Three subprocesses spawned in rapid succession each write a different
/// content to a different file. Stdout going to NUL must not multiplex
/// or interleave their writes (a file-channel side channel should
/// remain pristine).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_subprocesses_write_distinct_files() {
    let scope = format!("seam-concurrent-{}", line!());
    let (server_handle, socket, _tmp_dir) = start_server_with_tempdb(&scope);
    tokio::time::sleep(scaled(std::time::Duration::from_millis(300))).await;

    let workdir = tempfile::tempdir().expect("tempdir");
    let files: Vec<std::path::PathBuf> = (0..3)
        .map(|i| workdir.path().join(format!("out-{i}.txt")))
        .collect();
    let expected: Vec<String> = (0..3).map(|i| format!("payload-{i}")).collect();

    let socket_for_client = socket.clone();
    let files_for_client = files.clone();
    let expected_for_client = expected.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket_for_client).expect("connect");

        for (path, payload) in files_for_client.iter().zip(expected_for_client.iter()) {
            let path_str = path.to_string_lossy().into_owned();
            let command = if cfg!(windows) {
                format!("echo {payload}> \"{path_str}\"")
            } else {
                format!("printf '%s' '{payload}' > \"{path_str}\"")
            };
            let spawned = client
                .spawn_command(&SpawnCommandRequest::shell(command))
                .expect("spawn_command");
            assert!(spawned.pid > 0);
        }

        // Wait for all three marker files. Generous deadline.
        let deadline = std::time::Instant::now() + scaled(std::time::Duration::from_secs(10));
        while !files_for_client.iter().all(|p| p.exists()) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        for (path, expected) in files_for_client.iter().zip(expected_for_client.iter()) {
            let contents =
                std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
            assert!(
                contents.contains(expected),
                "expected {expected:?} in {path:?}, got {contents:?}"
            );
        }

        let _ = client.shutdown(true, 5.0);
    })
    .await;
    result.expect("client task");

    let _ = tokio::time::timeout(scaled(std::time::Duration::from_secs(5)), server_handle).await;
}

// ── 4. subprocess outlives daemon shutdown ──────────────────────────────────

/// The daemon shuts down BEFORE the subprocess finishes. Subprocess must
/// keep running (it was spawned detached) and complete its file-write.
/// This pins the "no pipe-back" trade-off: because there's no pipe from
/// subprocess to daemon, daemon shutdown can't cascade into the subprocess.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subprocess_outlives_daemon_shutdown_without_pipe_dangling() {
    let scope = format!("seam-outlives-{}", line!());
    let (server_handle, socket, _tmp_dir) = start_server_with_tempdb(&scope);
    tokio::time::sleep(scaled(std::time::Duration::from_millis(300))).await;

    let workdir = tempfile::tempdir().expect("tempdir");
    let marker = workdir.path().join("after-daemon-died.txt");
    let marker_str = marker.to_string_lossy().into_owned();

    // The subprocess sleeps long enough that we'll have killed the daemon
    // by the time it gets to writing the marker.
    let command = if cfg!(windows) {
        format!("ping 127.0.0.1 -n 3 >NUL & echo survived> \"{marker_str}\"")
    } else {
        format!("sleep 2; printf survived > \"{marker_str}\"")
    };

    let socket_for_client = socket.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket_for_client).expect("connect");
        let spawned = client
            .spawn_command(&SpawnCommandRequest::shell(command))
            .expect("spawn_command");
        assert!(spawned.pid > 0);

        // Shut down the daemon immediately — way before the sleep
        // expires inside the subprocess. If our spawn machinery held
        // any reader pipe open, this shutdown would block waiting
        // for it.
        let _ = client.shutdown(true, 5.0);
    })
    .await;
    result.expect("client task");

    // The server task must complete promptly because no pipe is hanging.
    tokio::time::timeout(scaled(std::time::Duration::from_secs(5)), server_handle)
        .await
        .expect("server did not stop in time")
        .expect("server task panicked");

    // Then the subprocess finishes on its own and writes the marker.
    let deadline = std::time::Instant::now() + scaled(std::time::Duration::from_secs(10));
    while !marker.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let contents = std::fs::read_to_string(&marker).expect("marker should exist");
    assert!(
        contents.contains("survived"),
        "subprocess marker has wrong content: {contents:?}"
    );
}

// ── 5. non-existent cwd is rejected, not silently rewritten ─────────────────

/// Hand the daemon a cwd that does not exist. The daemon must surface
/// the error from `CreateProcessW` / `execve` rather than silently
/// rewriting the cwd to the daemon's own and running the command there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subprocess_with_nonexistent_cwd_returns_spawn_error() {
    let scope = format!("seam-bogus-cwd-{}", line!());
    let (server_handle, socket, _tmp_dir) = start_server_with_tempdb(&scope);
    tokio::time::sleep(scaled(std::time::Duration::from_millis(300))).await;

    let bogus = if cfg!(windows) {
        // A drive letter we're confident is absent.
        std::path::PathBuf::from("Z:\\does\\not\\exist\\at\\all")
    } else {
        std::path::PathBuf::from("/this/does/not/exist/at/all")
    };

    let command = "echo should-not-run".to_string();

    let socket_for_client = socket.clone();
    let bogus_for_client = bogus.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket_for_client).expect("connect");
        let req = SpawnCommandRequest::shell(command).with_cwd(bogus_for_client);
        let outcome = client.spawn_command(&req);
        assert!(
            outcome.is_err(),
            "spawn with non-existent cwd should error, got {outcome:?}"
        );

        let _ = client.shutdown(true, 5.0);
    })
    .await;
    result.expect("client task");

    let _ = tokio::time::timeout(scaled(std::time::Duration::from_secs(5)), server_handle).await;
}

// ── 6. spawn response carries a positive pid even though stdio is NUL ───────

/// Sanity check that `SpawnCommandResponse.pid` is non-zero and the
/// process is in fact alive — the NUL-stdio policy mustn't accidentally
/// produce a zombie or skip the kernel-level spawn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_response_pid_is_alive() {
    let scope = format!("seam-pid-alive-{}", line!());
    let (server_handle, socket, _tmp_dir) = start_server_with_tempdb(&scope);
    tokio::time::sleep(scaled(std::time::Duration::from_millis(300))).await;

    let command = if cfg!(windows) {
        // ping keeps the process alive ~2s
        "ping 127.0.0.1 -n 3 >NUL".to_string()
    } else {
        "sleep 2".to_string()
    };

    let socket_for_client = socket.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket_for_client).expect("connect");
        let spawned = client
            .spawn_command(&SpawnCommandRequest::shell(command))
            .expect("spawn_command");
        assert!(spawned.pid > 0, "pid must be > 0, got {}", spawned.pid);

        // Immediately list — process should be visible in the registry
        // (no race here because the subprocess sleeps 2s before exiting).
        std::thread::sleep(scaled(std::time::Duration::from_millis(300)));
        let list_resp = client.list_active().expect("list_active");
        let processes = list_resp
            .list_active
            .expect("list_active payload")
            .processes;
        let tracked = processes
            .iter()
            .find(|p| p.pid == spawned.pid)
            .expect("spawned pid should be tracked");
        assert_eq!(tracked.containment, "detached");

        let kill_resp = client.kill_tree(spawned.pid, 3.0).expect("kill_tree");
        assert_eq!(kill_resp.code, StatusCode::Ok as i32);

        let _ = client.shutdown(true, 5.0);
    })
    .await;
    result.expect("client task");

    let _ = tokio::time::timeout(scaled(std::time::Duration::from_secs(5)), server_handle).await;
}
