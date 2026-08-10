"""Basic PTY behavior: round-trip I/O, command parsing, availability."""

from __future__ import annotations

import sys

import pytest

from running_process import (
    ExpectRule,
    RunningProcess,
)
from running_process.pty import Pty
from tests.pty._pty_helpers import _read_until_contains


def test_pseudo_terminal_round_trips_interactive_io() -> None:
    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            (
                "import sys; "
                "sys.stdout.write('ready>'); sys.stdout.flush(); "
                "line = sys.stdin.readline().strip(); "
                "sys.stdout.write(f'echo:{line}\\n'); sys.stdout.flush()"
            ),
        ],
        text=True,
    )

    initial = process.expect("ready>", timeout=5, action="hello from pty\n")
    assert initial.matched == "ready>"
    echoed = process.expect("echo:hello from pty", timeout=5)
    assert echoed.matched == "echo:hello from pty"
    assert process.wait(timeout=5) == 0


def test_pseudo_terminal_accepts_string_command_and_auto_splits() -> None:
    process = RunningProcess.pseudo_terminal(
        f"{sys.executable} -c \"print('string command ok')\"",
        text=True,
    )
    output = _read_until_contains(process, "string command ok")
    assert "string command ok" in output
    assert process.wait(timeout=5) == 0


def test_pseudo_terminal_preserves_carriage_returns() -> None:
    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            ("import sys, time; sys.stdout.write('first\\rsecond\\n'); sys.stdout.flush()"),
        ],
        text=True,
    )
    output = _read_until_contains(process, "second")
    assert "\r" in output
    process.wait(timeout=5)


def test_pseudo_terminal_expect_sequence_runs_during_creation() -> None:
    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            (
                "import sys; "
                "sys.stdout.write('name?'); sys.stdout.flush(); "
                "line = sys.stdin.readline().strip(); "
                "print('hello ' + line)"
            ),
        ],
        text=True,
        expect=[ExpectRule("name?", "pty user\n")],
        expect_timeout=5,
    )
    output = _read_until_contains(process, "hello pty user")
    assert "hello pty user" in output
    assert process.wait(timeout=5) == 0


def test_pseudo_terminal_expect_reports_pattern_not_found_on_eof() -> None:
    process = RunningProcess.pseudo_terminal([sys.executable, "-c", "print('done')"], text=True)
    with pytest.raises(EOFError, match="Pattern not found before stream closed"):
        process.expect("missing", timeout=5)


def test_pty_is_available_on_supported_platforms() -> None:
    if sys.platform in {"win32", "linux", "darwin"}:
        assert Pty.is_available() is True
    else:
        assert Pty.is_available() is False


def test_running_process_use_pty_remains_constructor_compatible() -> None:
    process = RunningProcess(
        [sys.executable, "-c", "print('pty compat')"],
        use_pty=True,
        capture=True,
    )
    assert process.wait(timeout=5) == 0
    assert b"pty compat" in process.stdout
