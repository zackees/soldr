from __future__ import annotations

import os
import subprocess
import sys

import pytest

from running_process import (
    DEVNULL,
    PIPE,
    CalledProcessError,
    CompletedProcess,
    ProcessAbnormalExit,
    RunningProcess,
    TimeoutExpired,
    subprocess_run,
)

live = pytest.mark.live
skip_unless_github_actions = pytest.mark.skipif(
    os.environ.get("GITHUB_ACTIONS", "").lower() != "true",
    reason="requires GitHub Actions runner",
)
pytestmark = [live, skip_unless_github_actions]


def test_run_rejects_unsupported_subprocess_kwargs() -> None:
    with pytest.raises(NotImplementedError, match="executable="):
        RunningProcess.run(
            [sys.executable, "-c", "print('x')"], executable=sys.executable
        )
    with pytest.raises(NotImplementedError, match="stdout=None or PIPE"):
        RunningProcess.run(
            [sys.executable, "-c", "print('x')"], stdout=subprocess.DEVNULL
        )
    with pytest.raises(NotImplementedError, match="extra Popen kwargs"):
        RunningProcess.run([sys.executable, "-c", "print('x')"], start_new_session=True)


def test_run_can_raise_on_abnormal_exit() -> None:
    with pytest.raises(ProcessAbnormalExit) as exc_info:
        RunningProcess.run(
            [sys.executable, "-c", "import sys; sys.exit(3)"],
            capture_output=True,
            raise_on_abnormal_exit=True,
        )
    assert exc_info.value.status.returncode == 3
    assert exc_info.value.status.abnormal is True


def test_run_matches_subprocess_contract() -> None:
    result = RunningProcess.run(
        [
            sys.executable,
            "-c",
            "import sys; print('out'); print('err', file=sys.stderr)",
        ],
        capture_output=True,
        check=True,
    )
    assert result.stdout is not None
    lines = result.stdout.strip().splitlines()
    assert "out" in lines
    assert "err" in lines
    assert result.stderr is None
    assert isinstance(result, CompletedProcess)
    assert isinstance(result, subprocess.CompletedProcess)


def test_run_can_explicitly_request_split_stderr() -> None:
    result = RunningProcess.run(
        [
            sys.executable,
            "-c",
            "import sys; print('out'); print('err', file=sys.stderr)",
        ],
        capture_output=True,
        stderr=subprocess.PIPE,
        text=True,
        check=True,
    )
    assert result.stdout == "out"
    assert result.stderr == "err"


def test_run_without_capture_returns_none_streams() -> None:
    result = RunningProcess.run([sys.executable, "-c", "print('out')"])
    assert result.stdout is None
    assert result.stderr is None


def test_run_capture_output_defaults_to_bytes() -> None:
    result = RunningProcess.run(
        [
            sys.executable,
            "-c",
            "import sys; sys.stdout.buffer.write(b'bad:\\xff'); sys.stderr.buffer.write(b'err')",
        ],
        capture_output=True,
        text=False,
    )
    assert result.stdout is not None
    assert b"bad:\xff" in result.stdout
    assert b"err" in result.stdout
    assert result.stderr is None


def test_run_capture_output_text_mode_replaces_invalid_utf8() -> None:
    result = RunningProcess.run(
        [
            sys.executable,
            "-c",
            (
                "import sys; "
                "sys.stdout.buffer.write(b'bad:\\xff'); "
                "sys.stderr.buffer.write(b'err:\\xfe')"
            ),
        ],
        capture_output=True,
        text=True,
    )
    assert result.stdout is not None
    lines = result.stdout.splitlines()
    assert "bad:�" in lines
    assert "err:�" in lines
    assert result.stderr is None


def test_run_preserves_bare_carriage_return_in_text_mode() -> None:
    result = RunningProcess.run(
        [
            sys.executable,
            "-c",
            "import sys; sys.stdout.buffer.write(b'a\\r\\nb\\rc\\n')",
        ],
        capture_output=True,
        text=True,
    )
    assert result.stdout == "a\nb\rc"


def test_run_timeout_raises_timeout_expired() -> None:
    with pytest.raises(TimeoutExpired) as exc_info:
        RunningProcess.run(
            [sys.executable, "-c", "import time; time.sleep(5)"],
            capture_output=True,
            timeout=0.2,
        )
    assert isinstance(exc_info.value, subprocess.TimeoutExpired)


def test_running_process_exports_subprocess_compat_constants() -> None:
    assert PIPE is subprocess.PIPE
    assert DEVNULL is subprocess.DEVNULL


def test_run_check_raises_compat_called_process_error() -> None:
    with pytest.raises(CalledProcessError) as exc_info:
        RunningProcess.run(
            [sys.executable, "-c", "import sys; sys.exit(9)"],
            capture_output=True,
            check=True,
        )
    assert isinstance(exc_info.value, subprocess.CalledProcessError)


def test_run_supports_detached_stdin_with_devnull() -> None:
    result = RunningProcess.run(
        [
            sys.executable,
            "-c",
            (
                "import sys; "
                "data = sys.stdin.read(); "
                "print('stdin_closed' if data == '' else 'stdin_open'); "
                "print('err-ok', file=sys.stderr)"
            ),
        ],
        stdin=subprocess.DEVNULL,
        capture_output=True,
        text=True,
        timeout=5,
    )
    assert result.stdout is not None
    lines = result.stdout.splitlines()
    assert "stdin_closed" in lines
    assert "err-ok" in lines
    assert result.stderr is None


def test_run_supports_shell_and_env() -> None:
    result = RunningProcess.run(
        "python -c \"import os; print(os.environ['RP_TEST_VALUE'])\"",
        shell=True,
        env={**os.environ, "RP_TEST_VALUE": "shell-ok"},
        capture_output=True,
        text=True,
        timeout=5,
    )
    assert result.stdout == "shell-ok"


def test_subprocess_run_capture_output() -> None:
    result = subprocess_run(
        command=[
            sys.executable,
            "-c",
            "import sys; print('out'); print('err', file=sys.stderr)",
        ],
        cwd=None,
        check=False,
        timeout=5,
    )
    assert result.stdout is not None
    lines = result.stdout.strip().splitlines()
    assert "out" in lines
    assert "err" in lines
    assert result.stderr is None


def test_subprocess_run_timeout_raises_runtime_error() -> None:
    with pytest.raises(RuntimeError, match="Process timed out"):
        subprocess_run(
            command=[sys.executable, "-c", "import time; time.sleep(5)"],
            cwd=None,
            check=False,
            timeout=0.1,  # type: ignore[arg-type]
        )


def test_run_streaming_echoes_both_streams(capsys: pytest.CaptureFixture[str]) -> None:
    code = RunningProcess.run_streaming(
        [
            sys.executable,
            "-c",
            "import sys; print('out'); print('err', file=sys.stderr)",
        ],
        timeout=5,
    )
    captured = capsys.readouterr()
    assert code == 0
    assert "out" in captured.out
    assert "err" in captured.out
    assert captured.err == ""


def test_run_streaming_stdout_callback_receives_output() -> None:
    lines: list[str] = []
    code = RunningProcess.run_streaming(
        [sys.executable, "-c", "print('hello'); print('world')"],
        timeout=5,
        stdout_callback=lines.append,
    )
    assert code == 0
    combined = "".join(lines)
    assert "hello" in combined
    assert "world" in combined


def test_run_streaming_stdout_callback_suppresses_console_echo(
    capsys: pytest.CaptureFixture[str],
) -> None:
    lines: list[str] = []
    RunningProcess.run_streaming(
        [sys.executable, "-c", "print('captured')"],
        timeout=5,
        stdout_callback=lines.append,
    )
    captured = capsys.readouterr()
    assert "captured" not in captured.out
    assert "captured" in "".join(lines)


def test_run_streaming_rejects_unknown_kwargs() -> None:
    with pytest.raises(TypeError):
        RunningProcess.run_streaming(
            [sys.executable, "-c", "print('x')"],
            bogus_kwarg="should fail",  # type: ignore[call-arg]
        )


def test_top_level_imports_preserve_output_formatter_exports() -> None:
    import running_process

    assert hasattr(running_process, "OutputFormatter")
    assert hasattr(running_process, "TimeDeltaFormatter")
