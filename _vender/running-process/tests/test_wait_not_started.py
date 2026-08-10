"""Reproduction and regression tests for calling wait() on unstarted processes.

Before 3.0, RunningProcess used subprocess.Popen which was created in start().
So checking `process.proc is None` was a valid way to detect "not started yet".

The Rust NativeProcess backend (3.0+) creates `self.proc` eagerly in __init__,
so `process.proc is None` is always False for non-PTY processes, even when
start() hasn't been called. Callers that relied on `proc is None` to gate
start() never called it, causing wait() to raise RuntimeError.

The fix: use `process.is_started` instead of `process.proc is None`.
"""

from __future__ import annotations

import os
import sys

import pytest

from running_process import RunningProcess

live = pytest.mark.live
skip_unless_github_actions = pytest.mark.skipif(
    os.environ.get("GITHUB_ACTIONS", "").lower() != "true",
    reason="requires GitHub Actions runner",
)
pytestmark = [live, skip_unless_github_actions]


def test_wait_on_unstarted_process_raises() -> None:
    """Calling wait() before start() must raise RuntimeError."""
    process = RunningProcess(
        [sys.executable, "-c", "print('hello')"],
        auto_run=False,
    )
    with pytest.raises(RuntimeError, match="process is not running"):
        process.wait()


def test_poll_on_unstarted_process_returns_none() -> None:
    """Calling poll() before start() should return None, not crash."""
    process = RunningProcess(
        [sys.executable, "-c", "print('hello')"],
        auto_run=False,
    )
    result = process.poll()
    assert result is None


def test_auto_run_false_then_start_then_wait_works() -> None:
    """The normal two-phase lifecycle: create -> start -> wait."""
    process = RunningProcess(
        [sys.executable, "-c", "print('hello')"],
        auto_run=False,
    )
    process.start()
    code = process.wait()
    assert code == 0


def test_double_wait_works() -> None:
    """Calling wait() twice should not crash — second call returns cached code."""
    process = RunningProcess([sys.executable, "-c", "print('hello')"])
    code1 = process.wait()
    code2 = process.wait()
    assert code1 == 0
    assert code2 == 0


def test_is_started_false_before_start() -> None:
    """is_started must be False when auto_run=False and start() not called."""
    process = RunningProcess(
        [sys.executable, "-c", "print('hello')"],
        auto_run=False,
    )
    assert process.is_started is False


def test_is_started_true_after_start() -> None:
    """is_started must be True after start() is called."""
    process = RunningProcess(
        [sys.executable, "-c", "print('hello')"],
        auto_run=False,
    )
    process.start()
    code = process.wait()
    assert code == 0
    assert process.is_started is True


def test_is_started_true_with_auto_run() -> None:
    """is_started must be True when auto_run=True (the default)."""
    process = RunningProcess([sys.executable, "-c", "print('hello')"])
    assert process.is_started is True
    process.wait()


def test_proc_is_none_when_unstarted() -> None:
    """proc is None before start(), matching the pre-Rust subprocess.Popen API.

    This restores backwards compatibility with code (e.g. FastLED's
    running_process_group.py) that checks `process.proc is None` to
    decide whether to call start().
    """
    process = RunningProcess(
        [sys.executable, "-c", "print('hello')"],
        auto_run=False,
    )
    # proc is None before start() — backwards compatible
    assert process.proc is None
    assert process.is_started is False

    # The `proc is None` guard works correctly now
    if process.proc is None:
        process.start()
    assert process.proc is not None
    code = process.wait()
    assert code == 0
