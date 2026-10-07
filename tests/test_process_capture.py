"""Real child-process coverage for the dependency-free capture path (#3528)."""

import socket
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import pytest
from conftest import load_script_module


def capture_module():
    return load_script_module(
        Path(__file__).resolve().parents[1] / "src/soldr/_process.py",
        "soldr_process_capture",
    )


def test_both_streams_larger_than_pipe_capacity():
    capture = capture_module()
    result = capture.run_captured(
        [
            sys.executable,
            "-c",
            "import sys; print('a'*262144); print('b'*262144, file=sys.stderr)",
        ],
        capture_output=True,
        text=True,
        timeout=10,
        check=True,
    )
    assert result.stdout == "a" * 262144 + "\n"
    assert result.stderr == "b" * 262144 + "\n"


def test_checked_output_is_bytes_by_default_and_text_when_requested():
    capture = capture_module()
    command = [sys.executable, "-c", "print('payload')"]
    assert capture.checked_output(command) == b"payload\n"
    assert capture.checked_output(command, text=True) == "payload\n"


def test_failed_command_retains_both_diagnostics():
    capture = capture_module()
    with pytest.raises(subprocess.CalledProcessError) as failed:
        capture.run_captured(
            [
                sys.executable,
                "-c",
                "import sys; print('partial'); print('failed',file=sys.stderr); sys.exit(7)",
            ],
            capture_output=True,
            text=True,
            check=True,
        )
    assert failed.value.returncode == 7
    assert failed.value.stdout == "partial\n"
    assert failed.value.stderr == "failed\n"


def test_large_input_is_read_from_a_regular_file():
    capture = capture_module()
    result = capture.run_captured(
        [sys.executable, "-c", "import sys; print(len(sys.stdin.read()))"],
        input="x" * 262144,
        capture_output=True,
        text=True,
        check=True,
    )
    assert result.stdout == "262144\n"


def test_timeout_still_raises_and_retains_output():
    capture = capture_module()
    with pytest.raises(subprocess.TimeoutExpired) as timed_out:
        capture.run_captured(
            [
                sys.executable,
                "-c",
                "import time; print('started',flush=True); time.sleep(30)",
            ],
            capture_output=True,
            timeout=1,
        )
    assert timed_out.value.output == b"started\n"


@pytest.mark.parametrize("stream", ["stdin", "stdout", "stderr"])
def test_capture_helper_cannot_be_used_as_a_pipe_escape(stream):
    capture = capture_module()
    with pytest.raises(ValueError, match="PIPE is unsupported"):
        capture.run_captured(
            [sys.executable, "-c", "pass"], **{stream: subprocess.PIPE}
        )


def test_capture_finishes_while_descendant_keeps_output_handles_open():
    """A broker-like descendant must not extend its parent's capture lifetime."""
    capture = capture_module()
    grandchild = (
        "import socket,sys; "
        "connection=socket.create_connection(('127.0.0.1',int(sys.argv[1]))); "
        "connection.sendall(b'ready'); connection.recv(1); connection.close()"
    )
    with socket.socket() as listener, ThreadPoolExecutor(max_workers=1) as workers:
        listener.bind(("127.0.0.1", 0))
        listener.listen(1)
        listener.settimeout(10)
        parent = (
            "import subprocess,sys; "
            f"subprocess.Popen([sys.executable,'-c',{grandchild!r},sys.argv[1]], "
            "stdin=subprocess.DEVNULL); print('parent finished',flush=True)"
        )
        completion = workers.submit(
            capture.run_captured,
            [sys.executable, "-c", parent, str(listener.getsockname()[1])],
            capture_output=True,
            text=True,
            check=True,
            timeout=10,
        )
        connection, _ = listener.accept()
        with connection:
            connection.settimeout(10)
            assert connection.recv(5) == b"ready"
            try:
                result = completion.result(timeout=3)
                assert result.stdout == "parent finished\n"
                assert result.stderr == ""
            finally:
                # Always release the descendant, including the pipe-based RED.
                connection.sendall(b"x")
            assert connection.recv(1) == b""


def test_errors_argument_alone_selects_text_capture():
    capture = capture_module()
    command = [sys.executable, "-c", "print('payload')"]
    result = capture.run_captured(command, capture_output=True, errors="replace")
    assert result.stdout == "payload\n"
    assert result.stderr == ""
    assert capture.checked_output(command, errors="replace") == "payload\n"


def test_checked_output_explicit_none_input_does_not_inherit_stdin(tmp_path):
    capture = capture_module()
    input_path = tmp_path / "parent-input"
    input_path.write_bytes(b"inherited")
    command = [sys.executable, "-c", "import sys; print(sys.stdin.read())"]
    with input_path.open("rb") as inherited:
        assert (
            capture.run_captured(command, stdin=inherited, capture_output=True).stdout
            == b"inherited\n"
        )
    with input_path.open("rb") as inherited:
        with pytest.raises(ValueError, match="stdin and input"):
            capture.checked_output(command, stdin=inherited, input=None)
    assert capture.checked_output(command, input=None) == b"\n"
