use std::time::{Duration, Instant};

use running_process::pty::{
    InteractivePtyOptions, InteractivePtySession, NativePtyProcess, PtyError,
};

fn python_available() -> bool {
    std::process::Command::new("python")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn python_command(script: &str) -> Vec<String> {
    vec!["python".into(), "-c".into(), script.into()]
}

#[cfg(windows)]
fn output_contains_windows_query_reply(output: &[u8]) -> bool {
    output
        .windows(b"\x1b[1;1R".len())
        .any(|window| window == b"\x1b[1;1R")
        || output
            .windows(b"^[[1;1R".len())
            .any(|window| window == b"^[[1;1R")
}

#[test]
fn interactive_pty_options_default_to_full_interactive_recipe() {
    let options = InteractivePtyOptions::default();

    assert!(options.echo_output);
    assert!(options.relay_terminal_input);
    assert!(options.respond_to_queries);
}

#[test]
fn start_terminal_input_relay_requires_running_pty() {
    let process = NativePtyProcess::new(python_command("print('ready')"), None, None, 24, 80, None)
        .expect("failed to create PTY process");
    let err = process
        .start_terminal_input_relay_impl()
        .expect_err("relay start should fail before PTY start");

    assert!(matches!(err, PtyError::NotRunning));
}

#[cfg(not(windows))]
#[test]
fn interactive_pty_session_pumps_output_and_waits_for_exit() {
    if !python_available() {
        eprintln!("[skip] python not on PATH");
        return;
    }

    let process = NativePtyProcess::new(
        python_command("print('hello from interactive session')"),
        None,
        None,
        24,
        80,
        None,
    )
    .expect("failed to create PTY process");
    let session = InteractivePtySession::with_options(
        process,
        InteractivePtyOptions {
            echo_output: false,
            relay_terminal_input: false,
            respond_to_queries: false,
        },
    );

    session
        .start()
        .expect("failed to start interactive PTY session");
    let mut output = Vec::new();
    let expected = b"hello from interactive session";
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let pumped = session
            .pump_output(Some(0.1), true)
            .expect("failed to pump PTY output");
        for chunk in pumped.chunks {
            output.extend_from_slice(&chunk);
        }
        if output
            .windows(expected.len())
            .any(|window| window == expected)
        {
            break;
        }
        if pumped.stream_closed {
            break;
        }
    }

    let code = session
        .wait_and_drain(Some(10.0), 2.0)
        .expect("failed to wait for PTY exit");
    assert_eq!(code, 0);

    let text = String::from_utf8_lossy(&output);
    assert!(
        text.contains("hello from interactive session"),
        "expected helper-drained PTY output, got {text:?}"
    );
}

#[cfg(windows)]
#[test]
fn interactive_pty_session_responds_to_terminal_queries() {
    if !python_available() {
        eprintln!("[skip] python not on PATH");
        return;
    }

    let process = NativePtyProcess::new(
        python_command(
            "import sys; \
             sys.stdout.buffer.write(b'\\x1b[6n'); \
             sys.stdout.buffer.flush(); \
             data = sys.stdin.buffer.read(6); \
             sys.stdout.buffer.write(b'reply=' + data); \
             sys.stdout.buffer.flush()",
        ),
        None,
        None,
        24,
        80,
        None,
    )
    .expect("failed to create PTY process");
    let session = InteractivePtySession::with_options(
        process,
        InteractivePtyOptions {
            echo_output: false,
            relay_terminal_input: false,
            respond_to_queries: true,
        },
    );
    session
        .start()
        .expect("failed to start interactive PTY session");

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut output = Vec::new();
    while Instant::now() < deadline {
        let pumped = session
            .pump_output(Some(0.1), true)
            .expect("failed to pump PTY output");
        for chunk in pumped.chunks {
            output.extend_from_slice(&chunk);
        }
        if output_contains_windows_query_reply(&output) {
            break;
        }
        if pumped.stream_closed {
            break;
        }
    }

    assert!(
        output_contains_windows_query_reply(&output),
        "expected Windows PTY query reply in output, got {:?}",
        String::from_utf8_lossy(&output)
    );
    let _ = session.wait(Some(5.0));
    let _ = session.close();
}
