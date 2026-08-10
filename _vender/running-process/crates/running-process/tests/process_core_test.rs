use std::time::{Duration, Instant};

#[cfg(windows)]
use std::env;
#[cfg(windows)]
use std::fs;
#[cfg(windows)]
use std::io::{BufRead, BufReader, Read, Write};
#[cfg(windows)]
use std::path::PathBuf;
#[cfg(any(windows, unix))]
use std::process::Command;
#[cfg(windows)]
use std::process::Stdio;
use std::thread;
#[cfg(target_os = "linux")]
use std::{ffi::OsString, os::unix::ffi::OsStringExt};

/// How long to wait for a child to exit.
///
/// Deliberately generous. These children are Python interpreters, and a cold
/// start under a parallel test run can take seconds before the script even
/// begins executing. What these tests assert is the exit status and the
/// captured output — never how quickly the interpreter got going.
///
/// 5s was too tight: `captures_stdout_and_stderr_separately_when_requested`,
/// `captured_combined_includes_both_streams`,
/// `normalizes_crlf_and_preserves_invalid_bytes` and
/// `has_pending_combined_reports_correctly` all failed on a loaded machine
/// during a parallel sweep (running-process#747).
///
/// Waits that deliberately expect a `Timeout` keep their own short bounds and
/// are untouched.
const CHILD_EXIT_WAIT: Duration = Duration::from_secs(30);

use running_process::{
    run_command, run_command_bounded, CommandSpec, NativeProcess, ProcessConfig, ProcessError,
    ReadStatus, StderrMode, StdinMode, StreamKind,
};

fn config(
    command: CommandSpec,
    capture: bool,
    stdin_mode: StdinMode,
    nice: Option<i32>,
) -> ProcessConfig {
    ProcessConfig {
        command,
        cwd: None,
        env: None,
        capture,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode,
        nice,
    }
}

#[test]
fn captures_stderr_in_stdout_by_default() {
    let process = NativeProcess::new(config(
        CommandSpec::Argv(vec![
            "python".into(),
            "-c".into(),
            "import sys; print('out'); print('err', file=sys.stderr)".into(),
        ]),
        true,
        StdinMode::Inherit,
        None,
    ));

    process.start().unwrap();
    let code = process.wait(Some(CHILD_EXIT_WAIT)).unwrap();

    assert_eq!(code, 0);
    assert!(process.captured_stdout().iter().any(|line| line == b"out"));
    assert!(process.captured_stdout().iter().any(|line| line == b"err"));
    assert!(process.captured_stderr().is_empty());
}

#[test]
fn run_command_returns_raw_output_and_exit_code() {
    let output = run_command(
        ProcessConfig {
            stderr_mode: StderrMode::Pipe,
            ..config(
                CommandSpec::Argv(vec![
                    "python".into(),
                    "-c".into(),
                    "import sys; sys.stdout.buffer.write(b'out\\n'); sys.stderr.buffer.write(b'err\\n'); sys.exit(7)"
                        .into(),
                ]),
                false,
                StdinMode::Null,
                None,
            )
        },
        Some(CHILD_EXIT_WAIT),
    )
    .unwrap();

    assert_eq!(output.exit_code, 7);
    assert_eq!(output.stdout, b"out\n");
    assert_eq!(output.stderr, b"err\n");
}

#[test]
fn run_command_drains_stdout_and_stderr_concurrently() {
    let output = run_command(
        ProcessConfig {
            stderr_mode: StderrMode::Pipe,
            ..config(
                CommandSpec::Argv(vec![
                    "python".into(),
                    "-c".into(),
                    "import sys; sys.stderr.buffer.write(b'e' * 262144); sys.stderr.flush(); sys.stdout.buffer.write(b'ok\\n'); sys.stdout.flush()"
                        .into(),
                ]),
                false,
                StdinMode::Null,
                None,
            )
        },
        Some(CHILD_EXIT_WAIT),
    )
    .unwrap();

    assert_eq!(output.exit_code, 0);
    assert_eq!(output.stdout, b"ok\n");
    assert_eq!(output.stderr.len(), 262144);
    assert!(output.stderr.iter().all(|byte| *byte == b'e'));
}

#[test]
fn run_command_timeout_kills_child_and_returns_timeout() {
    let started = Instant::now();
    let result = run_command(
        config(
            CommandSpec::Argv(vec![
                "python".into(),
                "-c".into(),
                "import time; time.sleep(10)".into(),
            ]),
            false,
            StdinMode::Null,
            None,
        ),
        Some(Duration::from_millis(100)),
    );

    assert!(matches!(result, Err(ProcessError::Timeout)));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "timeout path did not kill promptly"
    );
}

#[test]
fn bounded_run_stops_allocating_after_output_limit() {
    let result = run_command_bounded(
        ProcessConfig {
            stderr_mode: StderrMode::Pipe,
            ..config(
                CommandSpec::Argv(vec![
                    "python".into(),
                    "-c".into(),
                    "import sys; sys.stdout.buffer.write(b'x' * 4194304); sys.stdout.flush()"
                        .into(),
                ]),
                false,
                StdinMode::Null,
                None,
            )
        },
        Some(CHILD_EXIT_WAIT),
        1024,
    );

    assert!(matches!(
        result,
        Err(ProcessError::OutputLimitExceeded { limit: 1024 })
    ));
}

// Darwin's filesystem and process launch APIs reject these byte sequences
// with EILSEQ before exec; Linux accepts them and can exercise the lossless
// std::process::Command delegation end to end.
#[cfg(target_os = "linux")]
#[test]
fn bounded_std_command_preserves_non_utf8_process_inputs() {
    let temp = tempfile::tempdir().unwrap();
    let program = temp.path().join(OsString::from_vec(b"shell-\xff".to_vec()));
    std::os::unix::fs::symlink("/bin/sh", &program).unwrap();

    let mut command = Command::new(program);
    command
        .args(["-c", "printf %s \"$RP_BYTES\"; printf %s \"$1\"", "sh"])
        .arg(OsString::from_vec(b"arg-\xfe".to_vec()))
        .env("RP_BYTES", OsString::from_vec(b"environment-\xff".to_vec()));

    let output =
        running_process::run_std_command_bounded(command, Some(CHILD_EXIT_WAIT), 4096).unwrap();
    assert_eq!(output.stdout, b"environment-\xffarg-\xfe");
}

#[cfg(target_os = "linux")]
#[test]
fn bounded_run_cancels_readers_held_by_escaped_descendant() {
    let started = Instant::now();
    let result = run_command_bounded(
        config(
            CommandSpec::Argv(vec![
                "sh".into(),
                "-c".into(),
                "setsid sh -c 'sleep 3' & sleep 30".into(),
            ]),
            false,
            StdinMode::Null,
            None,
        ),
        Some(Duration::from_millis(100)),
        4096,
    );

    // `run_command_bounded` returns Timeout only after its cancelable reader
    // count reaches zero. A failed cancellation reports an I/O timeout.
    assert!(matches!(result, Err(ProcessError::Timeout)));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "escaped descendant kept capture readers alive for {:?}",
        started.elapsed()
    );
}

#[test]
fn captures_stdout_and_stderr_separately_when_requested() {
    let process = NativeProcess::new(ProcessConfig {
        stderr_mode: StderrMode::Pipe,
        ..config(
            CommandSpec::Argv(vec![
                "python".into(),
                "-c".into(),
                "import sys; print('out'); print('err', file=sys.stderr)".into(),
            ]),
            true,
            StdinMode::Inherit,
            None,
        )
    });

    process.start().unwrap();
    let code = process.wait(Some(CHILD_EXIT_WAIT)).unwrap();

    assert_eq!(code, 0);
    assert!(process.captured_stdout().iter().any(|line| line == b"out"));
    assert!(process.captured_stderr().iter().any(|line| line == b"err"));
}

#[test]
fn stream_reads_report_timeout_then_eof() {
    let process = NativeProcess::new(ProcessConfig {
        ..config(
            CommandSpec::Argv(vec![
                "python".into(),
                "-c".into(),
                "import time; time.sleep(0.2); print('ready')".into(),
            ]),
            true,
            StdinMode::Inherit,
            None,
        )
    });

    process.start().unwrap();
    assert_eq!(
        process.read_stream(StreamKind::Stdout, Some(Duration::from_millis(10))),
        ReadStatus::Timeout
    );
    // Generous, deliberately. What this asserts is that a read with time to
    // spare returns the line — not how fast a Python interpreter starts. The
    // child sleeps 200ms and then prints, but a cold interpreter under a
    // parallel test run can take seconds to reach that sleep, and a 2s bound
    // made this fail roughly 1 run in 10 (running-process#747).
    //
    // The 10ms timeout above is the real requirement and stays tight: it is
    // what proves a short deadline reports `Timeout` rather than blocking.
    assert!(matches!(
        process.read_stream(StreamKind::Stdout, Some(Duration::from_secs(30))),
        ReadStatus::Line(line) if line == b"ready"
    ));
    process.wait(Some(CHILD_EXIT_WAIT)).unwrap();
    assert_eq!(
        process.read_stream(StreamKind::Stdout, Some(Duration::from_millis(10))),
        ReadStatus::Eof
    );
}

#[test]
fn normalizes_crlf_and_preserves_invalid_bytes() {
    let process = NativeProcess::new(ProcessConfig {
        ..config(
            CommandSpec::Argv(vec![
                "python".into(),
                "-c".into(),
                "import sys; sys.stdout.buffer.write(b'bad:\\xff\\r\\nnext\\rthird\\n'); sys.stdout.flush()"
                    .into(),
            ]),
            true,
            StdinMode::Inherit,
            None,
        )
    });

    process.start().unwrap();
    let code = process.wait(Some(CHILD_EXIT_WAIT)).unwrap();

    assert_eq!(code, 0);
    assert_eq!(
        process.captured_stdout(),
        vec![b"bad:\xff".to_vec(), b"next\rthird".to_vec()]
    );
}

#[test]
fn supports_piped_stdin_filter_execution() {
    let process = NativeProcess::new(ProcessConfig {
        ..config(
            CommandSpec::Argv(vec![
                "python".into(),
                "-c".into(),
                "import sys; data = sys.stdin.buffer.read(); sys.stdout.buffer.write(data[::-1])"
                    .into(),
            ]),
            true,
            StdinMode::Piped,
            None,
        )
    });

    process.start().unwrap();
    process.write_stdin(b"abc").unwrap();
    let code = process.wait(Some(CHILD_EXIT_WAIT)).unwrap();

    assert_eq!(code, 0);
    assert_eq!(process.captured_stdout(), vec![b"cba".to_vec()]);
}

#[test]
fn captured_output_can_be_cleared_to_release_memory() {
    let process = NativeProcess::new(ProcessConfig {
        stderr_mode: StderrMode::Pipe,
        ..config(
            CommandSpec::Argv(vec![
                "python".into(),
                "-c".into(),
                "import sys; print('alpha'); print('beta', file=sys.stderr)".into(),
            ]),
            true,
            StdinMode::Inherit,
            None,
        )
    });

    process.start().unwrap();
    let code = process.wait(Some(CHILD_EXIT_WAIT)).unwrap();

    assert_eq!(code, 0);
    assert_eq!(process.captured_stream_bytes(StreamKind::Stdout), 5);
    assert_eq!(process.captured_stream_bytes(StreamKind::Stderr), 4);
    assert_eq!(process.captured_combined_bytes(), 9);
    assert_eq!(process.clear_captured_stream(StreamKind::Stdout), 5);
    assert!(process.captured_stdout().is_empty());
    assert_eq!(process.captured_stream_bytes(StreamKind::Stdout), 0);
    assert_eq!(process.clear_captured_combined(), 9);
    assert!(process.captured_combined().is_empty());
    assert_eq!(process.captured_combined_bytes(), 0);
}

#[test]
#[cfg(not(windows))]
fn applies_positive_nice_before_exec() {
    let process = NativeProcess::new(ProcessConfig {
        ..config(
            CommandSpec::Argv(vec![
                "python".into(),
                "-c".into(),
                "import os; print(os.nice(0))".into(),
            ]),
            true,
            StdinMode::Inherit,
            Some(5),
        )
    });

    process.start().unwrap();
    let code = process.wait(Some(CHILD_EXIT_WAIT)).unwrap();

    assert_eq!(code, 0);
    let observed = String::from_utf8(process.captured_stdout()[0].clone())
        .unwrap()
        .parse::<i32>()
        .unwrap();
    assert!(observed >= 5);
}

// ── Error path tests ──

#[test]
fn start_twice_returns_already_started() {
    let process = NativeProcess::new(config(
        CommandSpec::Argv(vec![
            "python".into(),
            "-c".into(),
            "import time; time.sleep(0.1)".into(),
        ]),
        false,
        StdinMode::Inherit,
        None,
    ));

    process.start().unwrap();
    assert!(matches!(process.start(), Err(ProcessError::AlreadyStarted)));
    let _ = process.kill();
}

#[test]
fn write_stdin_before_start_returns_not_running() {
    let process = NativeProcess::new(config(
        CommandSpec::Argv(vec!["python".into(), "-c".into(), "pass".into()]),
        false,
        StdinMode::Piped,
        None,
    ));

    assert!(matches!(
        process.write_stdin(b"hello"),
        Err(ProcessError::NotRunning)
    ));
}

#[test]
fn write_stdin_without_piped_returns_stdin_unavailable() {
    let process = NativeProcess::new(config(
        CommandSpec::Argv(vec![
            "python".into(),
            "-c".into(),
            "import time; time.sleep(0.1)".into(),
        ]),
        false,
        StdinMode::Inherit,
        None,
    ));

    process.start().unwrap();
    assert!(matches!(
        process.write_stdin(b"hello"),
        Err(ProcessError::StdinUnavailable)
    ));
    let _ = process.kill();
}

#[test]
fn kill_before_start_returns_not_running() {
    let process = NativeProcess::new(config(
        CommandSpec::Argv(vec!["python".into(), "-c".into(), "pass".into()]),
        false,
        StdinMode::Inherit,
        None,
    ));

    assert!(matches!(process.kill(), Err(ProcessError::NotRunning)));
}

#[test]
fn wait_before_start_returns_not_running() {
    let process = NativeProcess::new(config(
        CommandSpec::Argv(vec!["python".into(), "-c".into(), "pass".into()]),
        false,
        StdinMode::Inherit,
        None,
    ));

    assert!(matches!(
        process.wait(Some(Duration::from_secs(1))),
        Err(ProcessError::NotRunning)
    ));
}

#[test]
fn wait_timeout_returns_timeout_error() {
    let process = NativeProcess::new(config(
        CommandSpec::Argv(vec![
            "python".into(),
            "-c".into(),
            "import time; time.sleep(10)".into(),
        ]),
        false,
        StdinMode::Inherit,
        None,
    ));

    process.start().unwrap();
    assert!(matches!(
        process.wait(Some(Duration::from_millis(100))),
        Err(ProcessError::Timeout)
    ));
    let _ = process.kill();
}

// ── Combined stream tests ──

#[test]
fn read_combined_returns_events_from_both_streams() {
    let process = NativeProcess::new(ProcessConfig {
        stderr_mode: StderrMode::Pipe,
        ..config(
            CommandSpec::Argv(vec![
                "python".into(),
                "-c".into(),
                "import sys; print('out'); sys.stdout.flush(); print('err', file=sys.stderr); sys.stderr.flush()".into(),
            ]),
            true,
            StdinMode::Inherit,
            None,
        )
    });

    process.start().unwrap();
    process.wait(Some(CHILD_EXIT_WAIT)).unwrap();

    let mut events = Vec::new();
    loop {
        match process.read_combined(Some(Duration::from_millis(100))) {
            ReadStatus::Line(event) => events.push(event),
            ReadStatus::Eof => break,
            ReadStatus::Timeout => break,
        }
    }

    assert!(events
        .iter()
        .any(|e| e.stream == StreamKind::Stdout && e.line == b"out"));
    assert!(events
        .iter()
        .any(|e| e.stream == StreamKind::Stderr && e.line == b"err"));
}

#[test]
fn drain_combined_returns_all_pending() {
    let process = NativeProcess::new(ProcessConfig {
        stderr_mode: StderrMode::Pipe,
        ..config(
            CommandSpec::Argv(vec![
                "python".into(),
                "-c".into(),
                "import sys; print('a'); print('b', file=sys.stderr)".into(),
            ]),
            true,
            StdinMode::Inherit,
            None,
        )
    });

    process.start().unwrap();
    process.wait(Some(CHILD_EXIT_WAIT)).unwrap();
    // Small sleep to let reader threads finish queuing
    std::thread::sleep(Duration::from_millis(50));

    let events = process.drain_combined();
    assert!(events.len() >= 2);
}

#[test]
fn has_pending_combined_reports_correctly() {
    let process = NativeProcess::new(config(
        CommandSpec::Argv(vec!["python".into(), "-c".into(), "print('hello')".into()]),
        true,
        StdinMode::Inherit,
        None,
    ));

    process.start().unwrap();
    process.wait(Some(CHILD_EXIT_WAIT)).unwrap();

    // Wait for the reader thread to hand the child's output over, rather
    // than assuming 50ms is enough. The child exiting does not mean the
    // bytes have reached the combined buffer yet, and a fixed sleep made
    // this racy under a loaded parallel run.
    //
    // This is a bound, not a blank cheque: if the output never arrives the
    // loop expires and the assertion below still fails.
    let deadline = Instant::now() + CHILD_EXIT_WAIT;
    while !process.has_pending_combined() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(
        process.has_pending_combined(),
        "combined output never became pending"
    );
    process.drain_combined();
    assert!(!process.has_pending_combined());
}

#[test]
fn captured_combined_includes_both_streams() {
    let process = NativeProcess::new(ProcessConfig {
        stderr_mode: StderrMode::Pipe,
        ..config(
            CommandSpec::Argv(vec![
                "python".into(),
                "-c".into(),
                "import sys; print('out'); print('err', file=sys.stderr)".into(),
            ]),
            true,
            StdinMode::Inherit,
            None,
        )
    });

    process.start().unwrap();
    process.wait(Some(CHILD_EXIT_WAIT)).unwrap();

    let combined = process.captured_combined();
    assert!(combined
        .iter()
        .any(|e| e.stream == StreamKind::Stdout && e.line == b"out"));
    assert!(combined
        .iter()
        .any(|e| e.stream == StreamKind::Stderr && e.line == b"err"));
}

#[test]
fn captured_combined_bytes_and_clear() {
    let process = NativeProcess::new(ProcessConfig {
        stderr_mode: StderrMode::Pipe,
        ..config(
            CommandSpec::Argv(vec![
                "python".into(),
                "-c".into(),
                "import sys; print('ab'); print('cd', file=sys.stderr)".into(),
            ]),
            true,
            StdinMode::Inherit,
            None,
        )
    });

    process.start().unwrap();
    process.wait(Some(CHILD_EXIT_WAIT)).unwrap();

    assert_eq!(process.captured_combined_bytes(), 4);
    assert_eq!(process.clear_captured_combined(), 4);
    assert_eq!(process.captured_combined_bytes(), 0);
    assert!(process.captured_combined().is_empty());
}

// ── Shell command mode ──

#[test]
fn shell_command_captures_output() {
    let process = NativeProcess::new(config(
        CommandSpec::Shell("echo shell-works".into()),
        true,
        StdinMode::Inherit,
        None,
    ));

    process.start().unwrap();
    let code = process.wait(Some(CHILD_EXIT_WAIT)).unwrap();

    assert_eq!(code, 0);
    let stdout = process.captured_stdout();
    assert!(
        stdout.iter().any(|line| {
            let text = String::from_utf8_lossy(line);
            text.contains("shell-works")
        }),
        "expected 'shell-works' in output, got: {:?}",
        stdout,
    );
}

// ── Configuration: cwd and env ──

#[test]
fn custom_cwd_is_respected() {
    let tmp = std::env::temp_dir();
    let process = NativeProcess::new(ProcessConfig {
        cwd: Some(tmp.clone()),
        ..config(
            CommandSpec::Argv(vec![
                "python".into(),
                "-c".into(),
                "import os; print(os.getcwd())".into(),
            ]),
            true,
            StdinMode::Inherit,
            None,
        )
    });

    process.start().unwrap();
    let code = process.wait(Some(CHILD_EXIT_WAIT)).unwrap();

    assert_eq!(code, 0);
    let output = String::from_utf8(process.captured_stdout()[0].clone()).unwrap();
    // Canonicalize both for cross-platform comparison
    let expected = std::fs::canonicalize(&tmp).unwrap_or(tmp);
    let actual = std::fs::canonicalize(output.trim()).unwrap_or_else(|_| output.trim().into());
    assert_eq!(actual, expected);
}

#[test]
fn custom_env_is_applied() {
    // env_clear() wipes everything, so we must pass PATH for python to be found
    let mut env_vars = vec![("RP_TEST_VAR".into(), "hello_coverage".into())];
    if let Ok(path) = std::env::var("PATH") {
        env_vars.push(("PATH".into(), path));
    }
    // Python on Windows also needs SystemRoot for proper operation
    #[cfg(windows)]
    if let Ok(root) = std::env::var("SystemRoot") {
        env_vars.push(("SystemRoot".into(), root));
    }

    let process = NativeProcess::new(ProcessConfig {
        env: Some(env_vars),
        ..config(
            CommandSpec::Argv(vec![
                "python".into(),
                "-c".into(),
                "import os; print(os.environ.get('RP_TEST_VAR', 'MISSING'))".into(),
            ]),
            true,
            StdinMode::Inherit,
            None,
        )
    });

    process.start().unwrap();
    let code = process.wait(Some(CHILD_EXIT_WAIT)).unwrap();

    assert_eq!(code, 0);
    assert_eq!(process.captured_stdout(), vec![b"hello_coverage".to_vec()]);
}

// ── StdinMode::Null ──

#[test]
fn stdin_null_produces_empty_input() {
    let process = NativeProcess::new(config(
        CommandSpec::Argv(vec![
            "python".into(),
            "-c".into(),
            "import sys; data=sys.stdin.buffer.read(); print(len(data))".into(),
        ]),
        true,
        StdinMode::Null,
        None,
    ));

    process.start().unwrap();
    let code = process.wait(Some(CHILD_EXIT_WAIT)).unwrap();

    assert_eq!(code, 0);
    assert_eq!(process.captured_stdout(), vec![b"0".to_vec()]);
}

// ── poll() ──

#[test]
fn poll_returns_none_while_running_then_exit_code() {
    let process = NativeProcess::new(config(
        CommandSpec::Argv(vec![
            "python".into(),
            "-c".into(),
            "import time; time.sleep(0.3)".into(),
        ]),
        false,
        StdinMode::Inherit,
        None,
    ));

    process.start().unwrap();
    // Process should still be running
    let status = process.poll().unwrap();
    assert!(status.is_none(), "expected None, got {:?}", status);

    // Wait for it to finish
    process.wait(Some(CHILD_EXIT_WAIT)).unwrap();
    let status = process.poll().unwrap();
    assert_eq!(status, Some(0));
}

// ── close() and terminate() ──

#[test]
fn close_kills_running_process() {
    let process = NativeProcess::new(config(
        CommandSpec::Argv(vec![
            "python".into(),
            "-c".into(),
            "import time; time.sleep(0.1)".into(),
        ]),
        false,
        StdinMode::Inherit,
        None,
    ));

    process.start().unwrap();
    process.close().unwrap();
}

#[test]
fn close_on_already_finished_is_noop() {
    let process = NativeProcess::new(config(
        CommandSpec::Argv(vec!["python".into(), "-c".into(), "pass".into()]),
        false,
        StdinMode::Inherit,
        None,
    ));

    process.start().unwrap();
    process.wait(Some(CHILD_EXIT_WAIT)).unwrap();
    process.close().unwrap();
}

#[test]
fn terminate_kills_running_process() {
    let process = NativeProcess::new(config(
        CommandSpec::Argv(vec![
            "python".into(),
            "-c".into(),
            "import time; time.sleep(0.1)".into(),
        ]),
        false,
        StdinMode::Inherit,
        None,
    ));

    process.start().unwrap();
    process.terminate().unwrap();
}

/// FastLED Bug B follow-up: on Windows, `kill()` must wake the
/// reader threads via `CancelIoEx` *immediately* even when a
/// grandchild keeps the captured pipe open. This is what the
/// `cancel_capture_io()` call in `kill_impl` provides — without it,
/// kill() would wait for the full `RUNNING_PROCESS_KILL_DRAIN_TIMEOUT_MS`
/// safety-net deadline before returning. The test sets that deadline
/// to 5000 ms and asserts kill() returns in under 1 s, proving the
/// CancelIoEx fast path is wired up.
#[cfg(windows)]
#[test]
fn kill_cancels_capture_io_when_grandchild_orphans_pipe() {
    let script = "\
import os, subprocess, sys, time;\
print('PARENT_PID=' + str(os.getpid()), flush=True);\
gc = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)']);\
print('GRANDCHILD_PID=' + str(gc.pid), flush=True);\
time.sleep(60)";

    let process = NativeProcess::new(config(
        CommandSpec::Argv(vec!["python".into(), "-c".into(), script.into()]),
        true,
        StdinMode::Inherit,
        None,
    ));

    process.start().unwrap();

    let mut grandchild_pid: Option<u32> = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match process.read_combined(Some(Duration::from_millis(200))) {
            ReadStatus::Line(event) => {
                let line = String::from_utf8_lossy(&event.line).into_owned();
                if let Some(rest) = line.strip_prefix("GRANDCHILD_PID=") {
                    grandchild_pid = rest.trim().parse::<u32>().ok();
                    break;
                }
            }
            ReadStatus::Timeout => continue,
            ReadStatus::Eof => panic!("parent exited before announcing grandchild"),
        }
    }
    let grandchild_pid = grandchild_pid.expect("did not observe GRANDCHILD_PID line");
    // Crank the safety-net drain deadline way up so the only way
    // kill() can return fast is via the CancelIoEx fast path.
    let prior = env::var_os("RUNNING_PROCESS_KILL_DRAIN_TIMEOUT_MS");
    env::set_var("RUNNING_PROCESS_KILL_DRAIN_TIMEOUT_MS", "5000");

    let kill_start = Instant::now();
    let kill_result = process.kill();
    let kill_elapsed = kill_start.elapsed();

    match prior {
        Some(v) => env::set_var("RUNNING_PROCESS_KILL_DRAIN_TIMEOUT_MS", v),
        None => env::remove_var("RUNNING_PROCESS_KILL_DRAIN_TIMEOUT_MS"),
    }

    kill_result.expect("kill() returned an error");
    assert!(
        kill_elapsed < Duration::from_secs(1),
        "kill() took {kill_elapsed:?} with 5 s safety-net deadline; \
         CancelIoEx fast path is not interrupting the reader thread",
    );

    let _ = Command::new("taskkill")
        .args(["/F", "/T", "/PID", &grandchild_pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// FastLED Bug B regression: when a grandchild inherits the captured
/// stdout pipe and outlives the direct child, `kill()` must still
/// return promptly instead of blocking forever in
/// `wait_for_capture_completion`. Mirrors the `uv run python ...`
/// shape, where uv exits while a python grandchild keeps the pipe open.
#[test]
fn kill_returns_when_grandchild_inherits_stdout_pipe() {
    // Parent: print its own PID, spawn a grandchild python that sleeps
    // 60 s with inherited stdout, then itself sleep 60 s. We kill the
    // parent before either sleep elapses; the grandchild stays alive
    // (and thus the pipe stays open) for the duration of the test.
    let marker = format!(
        "running-process-619-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_nanos()
    );
    let grandchild_code = format!("import time; time.sleep(60) # {marker}");
    let script = format!(
        "\
import os, subprocess, sys, time;\
print('PARENT_PID=' + str(os.getpid()), flush=True);\
gc = subprocess.Popen([sys.executable, '-c', {grandchild_code:?}]);\
print('GRANDCHILD_PID=' + str(gc.pid), flush=True);\
time.sleep(60)"
    );

    let process_config = config(
        CommandSpec::Argv(vec!["python".into(), "-c".into(), script]),
        true,
        StdinMode::Inherit,
        None,
    );
    #[cfg(unix)]
    let process_config = ProcessConfig {
        create_process_group: true,
        ..process_config
    };
    let process = NativeProcess::new(process_config);

    process.start().unwrap();

    // Wait for the parent to spawn the grandchild and announce both PIDs.
    let mut grandchild_pid: Option<u32> = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match process.read_combined(Some(Duration::from_millis(200))) {
            ReadStatus::Line(event) => {
                let line = String::from_utf8_lossy(&event.line).into_owned();
                if let Some(rest) = line.strip_prefix("GRANDCHILD_PID=") {
                    grandchild_pid = rest.trim().parse::<u32>().ok();
                    break;
                }
            }
            ReadStatus::Timeout => continue,
            ReadStatus::Eof => panic!("parent exited before announcing grandchild"),
        }
    }
    let grandchild_pid = grandchild_pid.expect("did not observe GRANDCHILD_PID line");
    #[cfg(unix)]
    let is_original_grandchild_running = || {
        Command::new("ps")
            .args([
                "-ww",
                "-o",
                "stat=",
                "-o",
                "command=",
                "-p",
                &grandchild_pid.to_string(),
            ])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| {
                let state_and_command = String::from_utf8_lossy(&output.stdout);
                state_and_command.contains(&marker)
                    && !state_and_command.trim_start().starts_with('Z')
            })
            .unwrap_or(false)
    };
    #[cfg(unix)]
    assert!(
        is_original_grandchild_running(),
        "grandchild identity marker was not observable before kill"
    );

    // The grandchild now holds the stdout pipe open. kill() on the
    // parent reaps the parent but the reader thread is still blocked
    // on read(); without the bounded wait this hangs forever.
    let kill_start = Instant::now();
    process.kill().expect("kill() returned an error");
    let kill_elapsed = kill_start.elapsed();
    assert!(
        kill_elapsed < Duration::from_secs(5),
        "kill() blocked for {kill_elapsed:?}; expected bounded return after grandchild orphan",
    );

    // Cleanup: terminate the lingering grandchild so it doesn't leak.
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/F", "/T", "/PID", &grandchild_pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(unix)]
    {
        let gone_deadline = Instant::now() + Duration::from_secs(2);
        while is_original_grandchild_running() && Instant::now() < gone_deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let survived = is_original_grandchild_running();
        if survived {
            // The unique marker proves this PID still belongs to this test,
            // so emergency cleanup cannot signal a recycled unrelated PID.
            unsafe {
                libc::kill(grandchild_pid as i32, libc::SIGKILL);
            }
        }
        assert!(
            !survived,
            "grandchild {grandchild_pid} survived kill() on its owned process group"
        );
    }
}

/// Issue #590 (cluster A): when the direct child exits *on its own* (not
/// via `kill()`) while a grandchild inherited the captured stdout pipe
/// and outlives it, `wait()` must still return in bounded time. Before
/// the fix, the natural-exit path called an unbounded
/// `wait_for_capture_completion`, so `wait(Some(30s))` blocked forever —
/// the caller's timeout was silently defeated. Mirrors `uv run python
/// ...` where uv exits while a python grandchild keeps the pipe open.
#[test]
fn wait_returns_when_grandchild_orphans_pipe_on_natural_exit() {
    // Parent prints both PIDs, spawns a grandchild that sleeps 60 s with
    // the inherited stdout, then exits immediately. The grandchild keeps
    // the stdout pipe's write end open for the duration of the test, so
    // the reader thread never sees EOF on its own.
    let script = "\
import os, subprocess, sys;\
print('PARENT_PID=' + str(os.getpid()), flush=True);\
gc = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)']);\
print('GRANDCHILD_PID=' + str(gc.pid), flush=True);\
sys.stdout.flush()";

    let process = NativeProcess::new(config(
        CommandSpec::Argv(vec!["python".into(), "-c".into(), script.into()]),
        true,
        StdinMode::Inherit,
        None,
    ));

    process.start().unwrap();

    // Read until the parent announces the grandchild PID (it does so
    // before exiting; the held-open pipe means no premature EOF).
    let mut grandchild_pid: Option<u32> = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match process.read_combined(Some(Duration::from_millis(200))) {
            ReadStatus::Line(event) => {
                let line = String::from_utf8_lossy(&event.line).into_owned();
                if let Some(rest) = line.strip_prefix("GRANDCHILD_PID=") {
                    grandchild_pid = rest.trim().parse::<u32>().ok();
                    break;
                }
            }
            ReadStatus::Timeout => continue,
            ReadStatus::Eof => break,
        }
    }
    let grandchild_pid = grandchild_pid.expect("did not observe GRANDCHILD_PID line");

    // The parent has exited (or is about to); the grandchild holds the
    // stdout pipe open. Without the bounded natural-exit drain this
    // wait() blocks forever despite the requested 30 s timeout.
    let wait_start = Instant::now();
    let result = process.wait(Some(Duration::from_secs(30)));
    let wait_elapsed = wait_start.elapsed();

    // Reap the lingering grandchild first, so an assertion failure below
    // still cleans up.
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/F", "/T", "/PID", &grandchild_pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(windows))]
    unsafe {
        libc::kill(grandchild_pid as i32, libc::SIGKILL);
    }

    assert!(
        result.is_ok(),
        "wait() returned {result:?} instead of the child's exit code",
    );
    assert!(
        wait_elapsed < Duration::from_secs(10),
        "wait() blocked for {wait_elapsed:?}; the natural-exit capture drain \
         is not bounded (grandchild-orphaned pipe wedge, issue #590)",
    );
}

/// Issue #590 (cluster A) — drain-preservation guard. The bounded
/// natural-exit drain must NOT discard output a short-lived grandchild
/// writes *after* the direct child exits: unlike kill(), a natural exit
/// should let the reader drain within the grace window. This guards
/// against over-eagerly cancelling the reader on exit (which would drop
/// the late output, as the `stream_iter latches while stderr keeps
/// draining` behaviour relies on).
#[test]
fn natural_exit_still_captures_late_grandchild_output() {
    // Parent spawns a grandchild that, after a short delay, writes a
    // marker line to the inherited stdout and exits; the parent itself
    // exits immediately. The marker therefore lands on the pipe *after*
    // the direct child has already exited.
    let script = "\
import subprocess, sys;\
subprocess.Popen([sys.executable, '-c', \
\"import time,sys; time.sleep(0.3); print('LATE_GRANDCHILD_OUTPUT', flush=True); sys.stdout.flush()\"]);\
sys.exit(0)";

    // Use a generous drain grace so the behaviour under test (no eager
    // cancel of the reader on natural exit) is checked deterministically,
    // decoupled from grandchild-startup latency under CI load.
    let prior = std::env::var_os("RUNNING_PROCESS_KILL_DRAIN_TIMEOUT_MS");
    std::env::set_var("RUNNING_PROCESS_KILL_DRAIN_TIMEOUT_MS", "15000");

    let process = NativeProcess::new(config(
        CommandSpec::Argv(vec!["python".into(), "-c".into(), script.into()]),
        true,
        StdinMode::Inherit,
        None,
    ));

    process.start().unwrap();

    // Read events until EOF (or a generous bound) and collect them.
    let mut saw_late_output = false;
    let deadline = Instant::now() + Duration::from_secs(12);
    while Instant::now() < deadline {
        match process.read_combined(Some(Duration::from_millis(200))) {
            ReadStatus::Line(event) => {
                if String::from_utf8_lossy(&event.line).contains("LATE_GRANDCHILD_OUTPUT") {
                    saw_late_output = true;
                    break;
                }
            }
            ReadStatus::Timeout => continue,
            ReadStatus::Eof => break,
        }
    }

    let _ = process.wait(Some(Duration::from_secs(5)));

    match prior {
        Some(v) => std::env::set_var("RUNNING_PROCESS_KILL_DRAIN_TIMEOUT_MS", v),
        None => std::env::remove_var("RUNNING_PROCESS_KILL_DRAIN_TIMEOUT_MS"),
    }

    assert!(
        saw_late_output,
        "late grandchild output written after the direct child exited was \
         dropped; the natural-exit drain cancelled the reader too eagerly",
    );
}

// ── pid() ──

#[test]
fn pid_returns_some_after_start() {
    let process = NativeProcess::new(config(
        CommandSpec::Argv(vec![
            "python".into(),
            "-c".into(),
            "import time; time.sleep(0.1)".into(),
        ]),
        false,
        StdinMode::Inherit,
        None,
    ));

    assert!(process.pid().is_none());
    process.start().unwrap();
    assert!(process.pid().is_some());
    let _ = process.kill();
}

// ── process group (Unix) ──

#[test]
#[cfg(not(windows))]
fn create_process_group_sets_new_pgid() {
    let process = NativeProcess::new(ProcessConfig {
        create_process_group: true,
        ..config(
            CommandSpec::Argv(vec![
                "python".into(),
                "-c".into(),
                "import os; print(os.getpgid(0) == os.getpid())".into(),
            ]),
            true,
            StdinMode::Inherit,
            None,
        )
    });

    process.start().unwrap();
    let code = process.wait(Some(CHILD_EXIT_WAIT)).unwrap();

    assert_eq!(code, 0);
    assert_eq!(process.captured_stdout(), vec![b"True".to_vec()]);
}

// ── Windows tests ──

#[test]
#[cfg(windows)]
fn helper_force_killed_parent_reaps_native_child() {
    if env::var("RUNNING_PROCESS_CORE_HELPER").ok().as_deref() != Some("1") {
        return;
    }

    let process = NativeProcess::new(ProcessConfig {
        ..config(
            CommandSpec::Argv(vec![
                "python".into(),
                "-c".into(),
                "import time; time.sleep(0.1)".into(),
            ]),
            false,
            StdinMode::Inherit,
            None,
        )
    });
    process.start().unwrap();
    println!("CHILD_PID={}", process.pid().unwrap());
    std::io::stdout().flush().unwrap();
    thread::sleep(Duration::from_secs(30));
}

#[test]
#[cfg(windows)]
fn force_killed_parent_reaps_native_child_on_windows() {
    let current_exe = env::current_exe().unwrap();
    let mut owner = Command::new(current_exe)
        .arg("--exact")
        .arg("helper_force_killed_parent_reaps_native_child")
        .arg("--nocapture")
        .env("RUNNING_PROCESS_CORE_HELPER", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let child_pid = {
        let stdout = owner.stdout.take().unwrap();
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            let read = reader.read_line(&mut line).unwrap();
            assert!(read != 0, "helper exited before reporting child pid");
            if line.starts_with("CHILD_PID=") {
                break line
                    .trim()
                    .trim_start_matches("CHILD_PID=")
                    .parse::<u32>()
                    .unwrap();
            }
        }
    };

    owner.kill().unwrap();
    owner.wait().unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if !pid_exists(child_pid) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "child {child_pid} survived owner death"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
#[cfg(windows)]
fn helper_force_killed_parent_logs_native_child() {
    if env::var("RUNNING_PROCESS_CORE_HELPER_LOGGED")
        .ok()
        .as_deref()
        != Some("1")
    {
        return;
    }

    let process = NativeProcess::new(ProcessConfig {
        ..config(
            CommandSpec::Argv(vec![
                "python".into(),
                "-c".into(),
                "import time; time.sleep(0.1)".into(),
            ]),
            false,
            StdinMode::Inherit,
            None,
        )
    });
    process.start().unwrap();
    println!("OWNER_READY");
    std::io::stdout().flush().unwrap();
    thread::sleep(Duration::from_secs(30));
}

#[test]
#[cfg(windows)]
fn repeated_force_killed_parents_leave_no_logged_native_children_on_windows() {
    let current_exe = env::current_exe().unwrap();
    let log_path = unique_pid_log_path();
    let owner_count = if std::env::consts::ARCH == "aarch64" {
        4
    } else {
        6
    };
    let mut owners = Vec::new();

    for _ in 0..owner_count {
        let mut owner = Command::new(&current_exe)
            .arg("--exact")
            .arg("helper_force_killed_parent_logs_native_child")
            .arg("--nocapture")
            .env("RUNNING_PROCESS_CORE_HELPER_LOGGED", "1")
            .env("RUNNING_PROCESS_CHILD_PID_LOG_PATH", &log_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        {
            let stdout = owner.stdout.take().unwrap();
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                let read = reader.read_line(&mut line).unwrap();
                if read == 0 {
                    let mut stderr = String::new();
                    if let Some(stderr_pipe) = owner.stderr.as_mut() {
                        let _ = stderr_pipe.read_to_string(&mut stderr);
                    }
                    panic!(
                        "helper exited before reporting readiness; stderr:\n{}",
                        stderr.trim_end()
                    );
                }
                if line.trim() == "OWNER_READY" {
                    break;
                }
            }
        }

        owners.push(owner);
    }

    for owner in &mut owners {
        owner.kill().unwrap();
        owner.wait().unwrap();
    }

    let child_pids = read_logged_pids(&log_path);
    assert_eq!(child_pids.len(), owner_count);

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let all_dead = child_pids.iter().all(|pid| !pid_exists(*pid));
        if all_dead {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "some logged child pids survived owner death: {child_pids:?}"
        );
        thread::sleep(Duration::from_millis(50));
    }

    let _ = fs::remove_file(&log_path);
}

#[cfg(windows)]
fn unique_pid_log_path() -> PathBuf {
    let suffix = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    env::temp_dir().join(format!("running-process-native-child-pids-{suffix}.log"))
}

#[cfg(windows)]
fn read_logged_pids(path: &PathBuf) -> Vec<u32> {
    let content = fs::read_to_string(path).unwrap();
    content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| line.parse::<u32>().unwrap())
        .collect()
}

#[cfg(windows)]
fn pid_exists(pid: u32) -> bool {
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::processthreadsapi::{GetExitCodeProcess, OpenProcess};
    use winapi::um::winnt::PROCESS_QUERY_LIMITED_INFORMATION;

    const STILL_ACTIVE: u32 = 259;

    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }

    let mut exit_code = 0u32;
    let ok = unsafe { GetExitCodeProcess(handle, &mut exit_code) } != 0;
    unsafe {
        CloseHandle(handle);
    }
    ok && exit_code == STILL_ACTIVE
}

#[test]
fn returncode_auto_updates_without_poll() {
    let process = NativeProcess::new(config(
        CommandSpec::Argv(vec!["python".into(), "-c".into(), "print('hello')".into()]),
        true,
        StdinMode::Null,
        None,
    ));

    process.start().unwrap();

    // Wait up to 5 seconds for returncode to auto-update via the background waiter thread
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if process.returncode().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(
        process.returncode().is_some(),
        "returncode should auto-update via background waiter thread without calling poll()"
    );
    assert_eq!(process.returncode(), Some(0));
}
