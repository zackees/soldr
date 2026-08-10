"""PseudoTerminalProcess lifecycle: kill, terminate, gc, force-killed parent, interrupt_and_wait."""

from __future__ import annotations

import contextlib
import gc
import subprocess
import sys
from types import SimpleNamespace

import pytest

from running_process import RunningProcess
from running_process.pty import InterruptResult, PseudoTerminalProcess
from tests.process_helpers import pid_exists, wait_for_pid_exit


def test_pseudo_terminal_kill_and_terminate_are_idempotent_after_exit() -> None:
    process = RunningProcess.pseudo_terminal([sys.executable, "-c", "print('done')"], text=True)
    assert process.wait(timeout=5) == 0
    process.kill()
    process.terminate()


def test_pseudo_terminal_kill_reaps_child_process() -> None:
    process = RunningProcess.pseudo_terminal(
        [sys.executable, "-c", "import time; time.sleep(10)"],
        text=True,
    )
    pid = process.pid
    assert pid is not None

    process.kill()

    assert wait_for_pid_exit(pid, 3.0)
    assert process.poll() is not None
    assert not pid_exists(pid)


def test_pseudo_terminal_gc_reaps_child_process() -> None:
    process = RunningProcess.pseudo_terminal(
        [sys.executable, "-c", "import time; time.sleep(10)"],
        text=True,
    )
    pid = process.pid
    assert pid is not None

    del process
    gc.collect()

    assert wait_for_pid_exit(pid, 3.0, before_sleep=gc.collect)


def test_pseudo_terminal_force_killed_parent_reaps_child() -> None:
    if sys.platform != "win32":
        pytest.skip("Windows-specific PTY parent crash behavior")

    script = (
        "import sys, time\n"
        "from running_process import RunningProcess\n"
        "process = RunningProcess.pseudo_terminal(\n"
        "    [\n"
        "        sys.executable,\n"
        "        '-c',\n"
        "        \"import sys, time; sys.stdout.write('username:'); sys.stdout.flush(); "
        'sys.stdin.readline(); time.sleep(2)",\n'
        "    ],\n"
        "    text=True,\n"
        ")\n"
        "print(process.pid, flush=True)\n"
        "time.sleep(2)\n"
    )

    owner = subprocess.Popen(
        [sys.executable, "-c", script],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        assert owner.stdout is not None
        child_line = owner.stdout.readline().strip()
        assert child_line.isdigit(), f"expected PTY child pid, got: {child_line!r}"
        child_pid = int(child_line)

        owner.kill()
        owner.wait(timeout=5)

        assert wait_for_pid_exit(child_pid, 5.0)
    finally:
        with contextlib.suppress(Exception):
            owner.kill()
        with contextlib.suppress(Exception):
            owner.wait(timeout=1)


def test_interactive_force_killed_parent_reaps_child() -> None:
    if sys.platform != "win32":
        pytest.skip("Windows-specific parent crash behavior")

    script = (
        "import sys, time\n"
        "from running_process import InteractiveMode, RunningProcess\n"
        "process = RunningProcess.interactive(\n"
        "    [sys.executable, '-c', \"import time; time.sleep(2)\"],\n"
        "    mode=InteractiveMode.CONSOLE_ISOLATED,\n"
        ")\n"
        "print(process.pid, flush=True)\n"
        "time.sleep(2)\n"
    )

    owner = subprocess.Popen(
        [sys.executable, "-c", script],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        assert owner.stdout is not None
        child_pid = int(owner.stdout.readline().strip())

        owner.kill()
        owner.wait(timeout=5)

        assert wait_for_pid_exit(child_pid, 5.0)
    finally:
        with contextlib.suppress(Exception):
            owner.kill()
        with contextlib.suppress(Exception):
            owner.wait(timeout=1)


def test_pseudo_terminal_interrupt_and_wait_reports_second_interrupt_success(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    sent: list[str] = []
    waits = iter([False, True])

    process = PseudoTerminalProcess(
        [sys.executable, "-c", "print('x')"],
        auto_run=False,
    )
    process._proc = SimpleNamespace(send_interrupt=lambda: sent.append("interrupt"))
    monkeypatch.setattr(process, "_wait_until_exit", lambda timeout: next(waits))
    monkeypatch.setattr(process, "poll", lambda: 130 if len(sent) >= 2 else None)
    monkeypatch.setattr(process, "_drain_native_until_eof", lambda timeout: None)
    monkeypatch.setattr(process, "_finalize", lambda reason: None)

    result = process.interrupt_and_wait(grace_timeout=0.2, second_interrupt=True)

    assert isinstance(result, InterruptResult)
    assert sent == ["interrupt", "interrupt"]
    assert result.interrupt_count >= 2
    assert result.returncode == 130
    assert result.exit_reason == "interrupt"
    assert process.exit_reason == "interrupt"
