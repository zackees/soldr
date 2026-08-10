"""Capture flag behavior, text-mode warnings, and discard_output."""

from __future__ import annotations

import sys
import warnings

import pytest

from running_process import (
    IdleDetection,
    IdleTiming,
    RunningProcess,
)
from tests.pty._pty_helpers import _read_until_contains


def test_running_process_use_pty_text_mode_warns_and_falls_back_to_bytes() -> None:
    with pytest.MonkeyPatch.context() as monkeypatch:
        monkeypatch.delenv("RUNNING_PROCESS_NO_PTY_TEXT_WARNING", raising=False)
        with pytest.warns(
            RuntimeWarning,
            match="PTY mode ignores text/universal_newlines and always uses raw bytes",
        ):
            process = RunningProcess(
                [sys.executable, "-c", "print('warn')"],
                use_pty=True,
                text=True,
            )
    assert process.wait(timeout=5) == 0
    assert isinstance(process.stdout, bytes)


def test_running_process_use_pty_text_mode_warning_can_be_suppressed(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("RUNNING_PROCESS_NO_PTY_TEXT_WARNING", "1")
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        process = RunningProcess(
            [sys.executable, "-c", "print('quiet')"],
            use_pty=True,
            text=True,
        )
        assert process.wait(timeout=5) == 0
    runtime_warnings = [item for item in caught if issubclass(item.category, RuntimeWarning)]
    assert runtime_warnings == []


def test_pseudo_terminal_text_mode_warns_and_output_remains_bytes() -> None:
    with pytest.MonkeyPatch.context() as monkeypatch:
        monkeypatch.delenv("RUNNING_PROCESS_NO_PTY_TEXT_WARNING", raising=False)
        with pytest.warns(
            RuntimeWarning,
            match="PTY mode ignores text/universal_newlines and always uses raw bytes",
        ):
            process = RunningProcess.pseudo_terminal(
                [sys.executable, "-c", "print('warn')"],
                text=True,
            )
    assert process.wait(timeout=5) == 0
    assert isinstance(process.output, bytes)


def test_running_process_use_pty_defaults_to_no_capture() -> None:
    process = RunningProcess([sys.executable, "-c", "print('uncaptured')"], use_pty=True)

    assert process.wait(timeout=5) == 0
    assert process.stdout == b""
    assert process.stderr == b""
    assert process.combined_output == b""
    assert process.captured_output_bytes("stdout") == 0
    assert process.captured_output_bytes("combined") == 0


def test_running_process_use_pty_capture_true_retains_raw_bytes() -> None:
    process = RunningProcess(
        [sys.executable, "-c", "print('captured')"],
        use_pty=True,
        capture=True,
    )

    assert process.wait(timeout=5) == 0
    assert b"captured" in process.stdout


def test_running_process_use_pty_no_capture_still_supports_idle_detection() -> None:
    process = RunningProcess(
        [
            sys.executable,
            "-c",
            ("import sys, time\nsys.stdout.write('tick')\nsys.stdout.flush()\ntime.sleep(0.2)\n"),
        ],
        use_pty=True,
    )

    result = process.wait_for_idle(
        IdleDetection(
            timing=IdleTiming(
                timeout_seconds=0.05,
                stability_window_seconds=0.02,
                sample_interval_seconds=0.02,
            )
        ),
        timeout=1.0,
    )

    assert result.idle_detected is True
    assert process.stdout == b""
    assert process.captured_output_bytes("combined") == 0


def test_pseudo_terminal_discard_output_releases_history_bytes() -> None:
    process = RunningProcess.pseudo_terminal(
        [sys.executable, "-c", "print('alpha'); print('beta')"],
        text=True,
    )

    _read_until_contains(process, "beta")
    assert process.wait(timeout=5) == 0

    assert process.output_bytes >= len("alpha\nbeta")
    released = process.discard_output()
    assert released >= len("alpha\nbeta")
    assert process.output_bytes == 0
    assert process.output == b""
