from __future__ import annotations

import io
import os
import subprocess
import sys
import threading
import time

import pytest

from running_process import (
    EndOfStream,
    ProcessInfo,
    RunningProcess,
)
from running_process.exit_status import classify_exit_status

live = pytest.mark.live
skip_unless_github_actions = pytest.mark.skipif(
    os.environ.get("GITHUB_ACTIONS", "").lower() != "true",
    reason="requires GitHub Actions runner",
)
pytestmark = [live, skip_unless_github_actions]


def test_timeout_kills_process() -> None:
    process = RunningProcess(
        [sys.executable, "-c", "import time; time.sleep(10)"], timeout=1
    )
    with pytest.raises(TimeoutError):
        process.wait(timeout=1)
    assert process.finished


def test_terminate_finishes_process() -> None:
    process = RunningProcess([sys.executable, "-c", "import time; time.sleep(10)"])
    process.terminate()
    assert process.finished


def test_wait_uses_instance_timeout_and_callback() -> None:
    seen: list[ProcessInfo] = []
    process = RunningProcess(
        [sys.executable, "-c", "import time; time.sleep(10)"],
        timeout=0.1,
        on_timeout=seen.append,
    )
    with pytest.raises(TimeoutError):
        process.wait()
    assert len(seen) == 1
    assert seen[0].pid != 0
    assert seen[0].command == [sys.executable, "-c", "import time; time.sleep(10)"]


def test_non_blocking_line_reports_eos_immediately_after_timeout_kill() -> None:
    """Regression for the fastled non-blocking-EOS race.

    After `wait()` raises `TimeoutError` (which kills the child), the
    very next `get_next_line_non_blocking()` call must observe
    `EndOfStream` rather than `None`. The Rust `kill_impl` now
    synchronizes with the reader threads via
    `wait_for_capture_completion`, so by the time `kill()` returns the
    capture queues have flipped to "closed".
    """
    process = RunningProcess(
        [sys.executable, "-c", "import time; time.sleep(999)"],
        timeout=1,
    )
    with pytest.raises(TimeoutError):
        process.wait()
    nxt = process.get_next_line_non_blocking()
    assert isinstance(nxt, EndOfStream), f"expected EOS, got {nxt!r}"


def test_wait_raises_keyboard_interrupt_when_child_gets_sigint() -> None:
    creationflags = (
        getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0)
        if sys.platform == "win32"
        else None
    )
    process = RunningProcess(
        [
            sys.executable,
            "-c",
            (
                "import sys, time\n"
                "print('ready', flush=True)\n"
                "try:\n"
                "    time.sleep(2)\n"
                "except KeyboardInterrupt:\n"
                "    print('child-interrupted', flush=True)\n"
                "    raise\n"
            ),
        ],
        creationflags=creationflags,
        timeout=2,
    )
    assert process.get_next_stdout_line(timeout=5) == "ready"
    process.send_interrupt()
    with pytest.raises(KeyboardInterrupt):
        process.wait()


def test_wait_raises_keyboard_interrupt_promptly_while_main_thread_is_blocked() -> None:
    creationflags = (
        getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0)
        if sys.platform == "win32"
        else None
    )
    process = RunningProcess(
        [
            sys.executable,
            "-c",
            (
                "import time\n"
                "print('ready', flush=True)\n"
                "try:\n"
                "    time.sleep(2)\n"
                "except KeyboardInterrupt:\n"
                "    raise\n"
            ),
        ],
        creationflags=creationflags,
        timeout=5,
    )
    assert process.get_next_stdout_line(timeout=5) == "ready"

    sent_at: list[float] = []

    def trigger_interrupt() -> None:
        time.sleep(0.1)
        sent_at.append(time.perf_counter())
        process.send_interrupt()

    worker = threading.Thread(target=trigger_interrupt, daemon=True)
    worker.start()

    with pytest.raises(KeyboardInterrupt):
        process.wait()

    worker.join(timeout=1)
    assert sent_at

    # The regression this guards against is `wait()` not noticing the
    # interrupt at all and returning only when the child finishes — the child
    # sleeps 2s, so that is the failure signature. Anything comfortably under
    # that proves the interrupt was observed rather than waited out.
    #
    # The bound was 0.2s, which a shared CI runner missed by 14ms (#723);
    # scheduling jitter on a loaded macos-arm box is not evidence of an
    # interrupt-latency regression. 1.0s is still 2x under the child's sleep,
    # so the real failure mode remains caught.
    latency = time.perf_counter() - sent_at[0]
    assert latency < 1.0, f"interrupt took {latency:.3f}s; expected well under the child's 2s sleep"


def test_exit_status_classifies_possible_oom_for_sigkill_on_unix() -> None:
    status = classify_exit_status(-9, set(), platform="linux")
    assert status.signal_number == 9
    assert status.possible_oom is True
    assert status.abnormal is True


def test_exit_status_classifies_windows_no_memory_status() -> None:
    status = classify_exit_status(-1073741801, set(), platform="win32")
    assert status.possible_oom is True
    assert status.abnormal is True


def test_wait_echo_includes_stderr(capsys: pytest.CaptureFixture[str]) -> None:
    process = RunningProcess(
        [
            sys.executable,
            "-c",
            "import sys; print('out'); print('err', file=sys.stderr)",
        ]
    )
    process.wait(echo=True)
    captured = capsys.readouterr()
    assert "out" in captured.out
    assert "err" in captured.out
    assert captured.err == ""


def test_echo_true_is_safe_for_ascii_console(monkeypatch: pytest.MonkeyPatch) -> None:
    fake_stdout = io.TextIOWrapper(io.BytesIO(), encoding="ascii", errors="strict")
    monkeypatch.setattr(sys, "stdout", fake_stdout)
    process = RunningProcess([sys.executable, "-c", "print('snowman: \\u2603')"])
    process.wait(echo=True)
    fake_stdout.flush()
    assert b"snowman: ?" in fake_stdout.buffer.getvalue()
