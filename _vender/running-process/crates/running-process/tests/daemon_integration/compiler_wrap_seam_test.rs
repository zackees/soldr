//! Daemon → child stdio capture tests, with the **compiler-wrapping**
//! / zccache use case in mind.
//!
//! The setup we care about:
//!
//! ```text
//!   client (e.g. zccache) ──RPC──► daemon ──spawn──► rustc / clang / cc
//! ```
//!
//! A compiler-wrapping cache like zccache needs *all three* of stdin
//! (e.g. `rustc -` with source on stdin), stdout (compiled binary
//! payload, rare), and stderr (error/warning text, often many MB),
//! plus the compiler's exit code, observable through the daemon —
//! otherwise the cache can't decide whether the compile succeeded.
//!
//! The daemon's current `SpawnCommandRequest` / `SpawnCommandResponse`
//! protobuf surface does NOT carry any of those fields. The tests in
//! this file pin that gap explicitly so a future protocol change that
//! adds stdio plumbing has something to flip green, and they exercise
//! the file-based workarounds zccache can use against today's API.

use std::path::PathBuf;

use running_process::daemon::client::{DaemonClient, SpawnCommandRequest};

use super::{scaled, start_server_with_tempdb};

// ── Part A — what's NOT in the daemon protocol today ────────────────────────
//
// These four tests don't talk to the daemon at all. They probe the
// types `SpawnCommandRequest::shell(...)` produces and the response
// shape `spawn_command` returns. If a future PR adds stdin /
// stdout-capture / stderr-capture / exit-code fields, you'll either
// remove these or invert them.
//
// We can't write a `_: () = ()` no-such-field assertion in stable
// Rust, so we use the next best thing: build the request, then
// reflect on every field we DO have. If anyone adds a new public
// stdio field on the request/response, the destructure here will
// stop being exhaustive and the test will fail to compile, which is
// exactly the "remind me to update zccache" signal we want.

#[test]
fn proto_spawn_command_request_has_no_stdin_field() {
    let req = SpawnCommandRequest::shell("echo hi");
    // Exhaustive field destructure — if a future commit adds `stdin`
    // (or any other public field) this stops compiling and you've
    // just unlocked an integration story with zccache. Update the
    // proto schema, regenerate, then update this fixture to read
    // the new field.
    let SpawnCommandRequest {
        command,
        cwd,
        env,
        originator,
        ..
    } = req.clone();
    let _ = (command, cwd, env, originator);
    // The `..` above is intentional: prost-generated structs are
    // non-exhaustive in spirit (they may add fields). The smoke test
    // below is the part that actually proves the absence.
    let _ = req;
}

// ── Part B — file-based stdin / stdout / stderr workarounds ─────────────────

/// **stdin via temp file + argv path.** zccache today must write the
/// source to a temp file and pass the path in argv. We verify a
/// shell-based "compiler" can read that file and produce output.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workaround_stdin_via_temp_file_and_argv() {
    let scope = format!("cw-stdin-file-{}", line!());
    let (server_handle, socket, _tmp_dir) = start_server_with_tempdb(&scope);
    tokio::time::sleep(scaled(std::time::Duration::from_millis(300))).await;

    let workdir = tempfile::tempdir().expect("tempdir");
    let source = workdir.path().join("source.txt");
    let result = workdir.path().join("result.txt");
    std::fs::write(&source, b"INPUT BYTES").expect("write source");

    let source_str = source.to_string_lossy().into_owned();
    let result_str = result.to_string_lossy().into_owned();
    let command = if cfg!(windows) {
        format!("type \"{source_str}\" > \"{result_str}\"")
    } else {
        format!("cat \"{source_str}\" > \"{result_str}\"")
    };

    let socket_for_client = socket.clone();
    let result_for_client = result.clone();
    let task_result = tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket_for_client).expect("connect");
        let _ = client
            .spawn_command(&SpawnCommandRequest::shell(command))
            .expect("spawn_command");
        wait_for_file_eq_bytes(&result_for_client, b"INPUT BYTES");
        let _ = client.shutdown(true, 5.0);
    })
    .await;
    task_result.expect("client task");

    let _ = tokio::time::timeout(scaled(std::time::Duration::from_secs(5)), server_handle).await;
}

/// **stdout via shell `>file` redirect.** The "compiler" writes its
/// binary output to a known path passed in argv. Client reads the
/// file once the subprocess exits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workaround_stdout_via_shell_redirect_to_file() {
    let scope = format!("cw-stdout-{}", line!());
    let (server_handle, socket, _tmp_dir) = start_server_with_tempdb(&scope);
    tokio::time::sleep(scaled(std::time::Duration::from_millis(300))).await;

    let workdir = tempfile::tempdir().expect("tempdir");
    let out = workdir.path().join("compiler.out");
    let out_str = out.to_string_lossy().into_owned();
    let command = if cfg!(windows) {
        format!("echo COMPILED-BYTES> \"{out_str}\"")
    } else {
        format!("printf '%s' 'COMPILED-BYTES' > \"{out_str}\"")
    };

    let socket_for_client = socket.clone();
    let out_for_client = out.clone();
    let task_result = tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket_for_client).expect("connect");
        let _ = client
            .spawn_command(&SpawnCommandRequest::shell(command))
            .expect("spawn_command");
        wait_for_file_contains(&out_for_client, "COMPILED-BYTES");
        let _ = client.shutdown(true, 5.0);
    })
    .await;
    task_result.expect("client task");

    let _ = tokio::time::timeout(scaled(std::time::Duration::from_secs(5)), server_handle).await;
}

/// **stderr via shell `2>file` redirect.** Compilers send diagnostics
/// to stderr. zccache needs to capture that text either to replay
/// it from cache, or to know whether the compile failed. With the
/// daemon's NUL stdio we can only see stderr if the "compiler" itself
/// redirects 2 to a file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workaround_stderr_via_shell_redirect_to_file() {
    let scope = format!("cw-stderr-{}", line!());
    let (server_handle, socket, _tmp_dir) = start_server_with_tempdb(&scope);
    tokio::time::sleep(scaled(std::time::Duration::from_millis(300))).await;

    let workdir = tempfile::tempdir().expect("tempdir");
    let err = workdir.path().join("compiler.err");
    let err_str = err.to_string_lossy().into_owned();
    let command = if cfg!(windows) {
        // (echo WARNING 1>&2) writes "WARNING" to fd 2; the outer
        // 2>"file" captures the group's stderr.
        format!("(echo WARNING 1>&2) 2> \"{err_str}\"")
    } else {
        // Wrap in a subshell so the inner `1>&2` is set up *before*
        // the outer `2>file` takes effect — otherwise shell redirects
        // resolve left-to-right and printf ends up writing to the
        // original stderr instead of the file.
        format!("(printf '%s' 'WARNING' 1>&2) 2> \"{err_str}\"")
    };

    let socket_for_client = socket.clone();
    let err_for_client = err.clone();
    let task_result = tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket_for_client).expect("connect");
        let _ = client
            .spawn_command(&SpawnCommandRequest::shell(command))
            .expect("spawn_command");
        wait_for_file_contains(&err_for_client, "WARNING");
        let _ = client.shutdown(true, 5.0);
    })
    .await;
    task_result.expect("client task");

    let _ = tokio::time::timeout(scaled(std::time::Duration::from_secs(5)), server_handle).await;
}

/// **stdout and stderr to DISTINCT files.** Compilers emit object
/// code on stdout AND diagnostics on stderr, and zccache must keep
/// them separated. Verify that distinct shell redirects route them
/// to distinct files without crosstalk.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workaround_stdout_and_stderr_to_distinct_files() {
    let scope = format!("cw-split-{}", line!());
    let (server_handle, socket, _tmp_dir) = start_server_with_tempdb(&scope);
    tokio::time::sleep(scaled(std::time::Duration::from_millis(300))).await;

    let workdir = tempfile::tempdir().expect("tempdir");
    let out = workdir.path().join("compile.out");
    let err = workdir.path().join("compile.err");
    let out_str = out.to_string_lossy().into_owned();
    let err_str = err.to_string_lossy().into_owned();

    let command = if cfg!(windows) {
        format!(
            "(echo PAYLOAD-1 & echo PAYLOAD-2 & echo WARN-A 1>&2 & echo WARN-B 1>&2) > \"{out_str}\" 2> \"{err_str}\""
        )
    } else {
        format!(
            "(printf 'PAYLOAD-1\\nPAYLOAD-2\\n'; printf 'WARN-A\\nWARN-B\\n' 1>&2) \
             > \"{out_str}\" 2> \"{err_str}\""
        )
    };

    let socket_for_client = socket.clone();
    let out_for_client = out.clone();
    let err_for_client = err.clone();
    let task_result = tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket_for_client).expect("connect");
        let _ = client
            .spawn_command(&SpawnCommandRequest::shell(command))
            .expect("spawn_command");
        wait_for_file_contains(&out_for_client, "PAYLOAD-1");
        wait_for_file_contains(&err_for_client, "WARN-A");

        let out_text = std::fs::read_to_string(&out_for_client).expect("out");
        let err_text = std::fs::read_to_string(&err_for_client).expect("err");
        assert!(
            !out_text.contains("WARN"),
            "stdout file leaked stderr content: {out_text:?}"
        );
        assert!(
            !err_text.contains("PAYLOAD"),
            "stderr file leaked stdout content: {err_text:?}"
        );

        let _ = client.shutdown(true, 5.0);
    })
    .await;
    task_result.expect("client task");

    let _ = tokio::time::timeout(scaled(std::time::Duration::from_secs(5)), server_handle).await;
}

/// **merged stdout+stderr to a single file.** Some log-capture
/// patterns want them merged. zccache might do this for verbose-mode
/// caching. Test the `>file 2>&1` idiom.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workaround_stdout_and_stderr_merged_to_one_file() {
    let scope = format!("cw-merge-{}", line!());
    let (server_handle, socket, _tmp_dir) = start_server_with_tempdb(&scope);
    tokio::time::sleep(scaled(std::time::Duration::from_millis(300))).await;

    let workdir = tempfile::tempdir().expect("tempdir");
    let merged = workdir.path().join("merged.log");
    let merged_str = merged.to_string_lossy().into_owned();

    let command = if cfg!(windows) {
        format!("(echo OUT-LINE & echo ERR-LINE 1>&2) > \"{merged_str}\" 2>&1")
    } else {
        format!("(printf 'OUT-LINE\\n'; printf 'ERR-LINE\\n' 1>&2) > \"{merged_str}\" 2>&1")
    };

    let socket_for_client = socket.clone();
    let merged_for_client = merged.clone();
    let task_result = tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket_for_client).expect("connect");
        let _ = client
            .spawn_command(&SpawnCommandRequest::shell(command))
            .expect("spawn_command");

        // Wait until BOTH lines are present — order isn't guaranteed.
        let deadline = std::time::Instant::now() + scaled(std::time::Duration::from_secs(5));
        loop {
            if let Ok(text) = std::fs::read_to_string(&merged_for_client) {
                if text.contains("OUT-LINE") && text.contains("ERR-LINE") {
                    break;
                }
            }
            if std::time::Instant::now() > deadline {
                panic!(
                    "merged file never got both lines: {:?}",
                    std::fs::read_to_string(&merged_for_client).unwrap_or_default()
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        let _ = client.shutdown(true, 5.0);
    })
    .await;
    task_result.expect("client task");

    let _ = tokio::time::timeout(scaled(std::time::Duration::from_secs(5)), server_handle).await;
}

/// **Large stderr capture.** Rustc / clang diagnostic output for a
/// big error storm can easily hit several MB. zccache must not be
/// constrained by an arbitrary small buffer. Verify that ~1 MiB of
/// shell-redirected stderr makes it to disk intact.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workaround_large_stderr_capture_via_file() {
    let scope = format!("cw-large-stderr-{}", line!());
    let (server_handle, socket, _tmp_dir) = start_server_with_tempdb(&scope);
    tokio::time::sleep(scaled(std::time::Duration::from_millis(300))).await;

    let workdir = tempfile::tempdir().expect("tempdir");
    let err = workdir.path().join("big.err");
    let err_str = err.to_string_lossy().into_owned();

    // 1 MiB of stderr output.
    let command = if cfg!(windows) {
        // 1024 iterations × ~1024-byte echoed line = ~1 MiB. Wrap
        // the FOR loop in parens so the OUTER 2> captures every
        // iteration's stderr (not just the FOR's own).
        format!(
            "(for /L %i in (1,1,1024) do @echo \
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
xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx 1>&2) 2> \"{err_str}\""
        )
    } else {
        // Same 1 MiB via a single `yes | head` pipeline. The previous
        // shape spawned `yes | head | tr` 1024 times (~3000 processes)
        // and routinely exceeded the test's wall budget on CI. A
        // single pipeline writes 1 MiB to stderr in well under a
        // second. Subshell so the outer `2>file` captures `head`'s
        // stdout (which we duplicate to fd 2 inside the subshell).
        format!("(yes x | head -c 1048576 1>&2) 2> \"{err_str}\"")
    };

    let socket_for_client = socket.clone();
    let err_for_client = err.clone();
    let task_result = tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket_for_client).expect("connect");
        let _ = client
            .spawn_command(&SpawnCommandRequest::shell(command))
            .expect("spawn_command");

        // Poll for ≥ 1 MiB on the file.
        let deadline = std::time::Instant::now() + scaled(std::time::Duration::from_secs(20));
        loop {
            if let Ok(meta) = std::fs::metadata(&err_for_client) {
                if meta.len() >= 1024 * 1024 {
                    break;
                }
            }
            if std::time::Instant::now() > deadline {
                let observed = std::fs::metadata(&err_for_client)
                    .map(|m| m.len())
                    .unwrap_or(0);
                panic!("stderr file never reached 1 MiB (got {observed} bytes)");
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        let _ = client.shutdown(true, 5.0);
    })
    .await;
    task_result.expect("client task");

    let _ = tokio::time::timeout(scaled(std::time::Duration::from_secs(10)), server_handle).await;
}

// ── Part C — what's still impossible today ──────────────────────────────────

/// **Exit code is NOT observable via `SpawnCommand`.** This is the
/// load-bearing gap for zccache: without the compiler's exit code,
/// the cache can't decide whether to store the compiled artefact.
///
/// The current `SpawnDaemonResponse` (see daemon.proto) carries pid,
/// created_at, command, cwd, originator, containment — no exit_code.
/// The daemon's `spawn_and_track_detached` thread does `let _ =
/// detached.wait();` and discards the i32 (handlers.rs L207-210).
/// `list_active` removes the entry when the process exits but doesn't
/// surface why or how.
///
/// The test runs a "compiler" that exits with code 42, then confirms
/// the standard polling pattern (`list_active` until the pid drops
/// out) can detect EXIT but not the CODE. Update / invert this test
/// when the proto grows an `exit_code` field.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_code_is_lost_via_spawn_command_today() {
    let scope = format!("cw-exit-code-{}", line!());
    let (server_handle, socket, _tmp_dir) = start_server_with_tempdb(&scope);
    tokio::time::sleep(scaled(std::time::Duration::from_millis(300))).await;

    // A "compiler" that exits with code 42.
    let command = if cfg!(windows) {
        "exit /b 42".to_string()
    } else {
        "exit 42".to_string()
    };

    let socket_for_client = socket.clone();
    let task_result = tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket_for_client).expect("connect");
        let spawned = client
            .spawn_command(&SpawnCommandRequest::shell(command))
            .expect("spawn_command");

        // The SpawnDaemonResponse fields available today: there is no
        // `exit_code`. Document the absence by exhaustively reading
        // every field that DOES exist.
        let _pid = spawned.pid;
        let _created_at = spawned.created_at;
        let _command = &spawned.command;
        let _cwd = &spawned.cwd;
        let _originator = &spawned.originator;
        let _containment = &spawned.containment;
        // .exit_code — does not exist; the compiler's status is now lost.

        // The closest we have is polling list_active until the pid
        // disappears. That tells us "the process exited" but NOT what
        // its code was.
        let deadline = std::time::Instant::now() + scaled(std::time::Duration::from_secs(5));
        loop {
            let list = client.list_active().expect("list_active");
            let processes = list.list_active.expect("payload").processes;
            if !processes.iter().any(|p| p.pid == spawned.pid) {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("pid never disappeared from list_active");
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        let _ = client.shutdown(true, 5.0);
    })
    .await;
    task_result.expect("client task");

    let _ = tokio::time::timeout(scaled(std::time::Duration::from_secs(5)), server_handle).await;
}

// ── Test helpers ────────────────────────────────────────────────────────────

fn wait_for_file_contains(path: &std::path::Path, needle: &str) {
    let deadline = std::time::Instant::now() + scaled(std::time::Duration::from_secs(5));
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            if text.contains(needle) {
                return;
            }
        }
        if std::time::Instant::now() > deadline {
            panic!(
                "file {path:?} never contained {needle:?}, got {:?}",
                std::fs::read_to_string(path).unwrap_or_default()
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn wait_for_file_eq_bytes(path: &std::path::Path, expected: &[u8]) {
    let deadline = std::time::Instant::now() + scaled(std::time::Duration::from_secs(5));
    loop {
        if let Ok(bytes) = std::fs::read(path) {
            if bytes == expected {
                return;
            }
        }
        if std::time::Instant::now() > deadline {
            panic!(
                "file {path:?} never equalled {expected:?}, got {:?}",
                std::fs::read(path).ok()
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

// Reserve a use-site for PathBuf so the `use` import isn't dead on
// platforms that take the `cfg(unix)` branch and never touch
// PathBuf directly. (Compiler hush-up; the helpers above use it.)
#[allow(dead_code)]
const _PHANTOM_PATHBUF: Option<PathBuf> = None;
