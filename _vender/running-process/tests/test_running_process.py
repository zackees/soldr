from __future__ import annotations

import os
import sys
import time

import pytest

from running_process import RunningProcess

live = pytest.mark.live
skip_unless_github_actions = pytest.mark.skipif(
    os.environ.get("GITHUB_ACTIONS", "").lower() != "true",
    reason="requires GitHub Actions runner",
)
pytestmark = [live, skip_unless_github_actions]


def test_finished_becomes_true_without_poll() -> None:
    """Regression test for issue #7: .finished never becomes True without explicit .poll()."""
    timeout = 10
    process = RunningProcess([sys.executable, "-c", "print('hello')"])
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process.finished:
            break
        time.sleep(0.05)
    assert (
        process.finished
    ), "Process.finished never became True without explicit poll()"
    assert process.returncode == 0


def test_capture_false_does_not_store_output() -> None:
    process = RunningProcess([sys.executable, "-c", "print('hidden')"], capture=False)
    process.wait()
    assert process.stdout == ""
    assert process.stderr == ""


def test_invalid_string_command_without_shell() -> None:
    with pytest.raises(ValueError, match="String commands require shell=True"):
        RunningProcess("echo nope", shell=False)


def test_invalid_echo_type_raises() -> None:
    process = RunningProcess([sys.executable, "-c", "print('hello')"])
    with pytest.raises(TypeError):
        process.wait(echo="bad")  # type: ignore[arg-type]


def test_is_runninng_compat_alias() -> None:
    process = RunningProcess([sys.executable, "-c", "import time; time.sleep(0.2)"])
    assert process.is_runninng() is True
    process.wait()
    assert process.is_runninng() is False


def test_echo_true_writes_stdout_only(capsys: pytest.CaptureFixture[str]) -> None:
    process = RunningProcess([sys.executable, "-c", "print('hello')"])
    process.wait(echo=True)
    captured = capsys.readouterr()
    assert "hello" in captured.out
