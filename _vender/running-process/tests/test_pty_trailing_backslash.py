"""Regression test for trailing backslash in PTY output.

Reproduces an issue where the PTY echoes input lines with a trailing
backslash character appended.  This can happen when ConPTY wraps text
at the column boundary or when input payload translation introduces
spurious bytes.
"""
from __future__ import annotations

import os
import re
import sys
import time
import unittest

import pytest
from running_process._native import native_windows_terminal_input_bytes

from running_process import RunningProcess

BACKSLASH = 0x5C  # ord("\\")
live = pytest.mark.live
skip_unless_github_actions = pytest.mark.skipif(
    os.environ.get("GITHUB_ACTIONS", "").lower() != "true",
    reason="requires GitHub Actions runner",
)
skip_unless_dedicated_gh_pty_runner = pytest.mark.skipif(
    os.environ.get("RUNNING_PROCESS_GH_PTY_TESTS") != "1",
    reason="requires dedicated GitHub Actions PTY integration runner",
)


def _read_until_contains(process: object, needle: str, timeout: float = 10) -> str:
    deadline = time.time() + timeout
    chunks: list[str] = []
    while time.time() < deadline:
        try:
            chunk = process.read(timeout=0.2)  # type: ignore[attr-defined]
        except TimeoutError:
            continue
        except EOFError:
            break
        text = chunk.decode("utf-8", errors="replace") if isinstance(chunk, bytes) else chunk
        chunks.append(text)
        if needle in "".join(chunks):
            return "".join(chunks)
    raise AssertionError(f"Did not observe {needle!r} in PTY output: {''.join(chunks)!r}")



def _strip_ansi(text: str) -> str:
    """Remove ANSI escape sequences from text."""
    return re.sub(r"\x1b\[[0-9;]*[A-Za-z]|\x1b\][^\x07]*\x07|\x1b.", "", text)


def _assert_no_trailing_backslash(output: str, context: str) -> None:
    """Check that no visible output line ends with a spurious backslash.

    Strips ANSI escape sequences first so that control churn does not
    interfere with the check.
    """
    clean = _strip_ansi(output)
    for i, line in enumerate(clean.splitlines()):
        stripped = line.rstrip()
        if not stripped:
            continue
        assert not stripped.endswith("\\"), (
            f"Line {i} ends with trailing backslash ({context}):\n"
            f"  clean line: {line!r}\n"
            f"  raw output: {output!r}"
        )


# ── Unit tests for the input payload transformation ───────────────────────


class TestInputPayloadNoBackslash(unittest.TestCase):
    """Verify that windows_terminal_input_payload never introduces backslashes."""

    def test_newline_does_not_produce_backslash(self) -> None:
        result = native_windows_terminal_input_bytes(b"\n")
        assert BACKSLASH not in result, f"Payload contains backslash: {result!r}"

    def test_carriage_return_does_not_produce_backslash(self) -> None:
        result = native_windows_terminal_input_bytes(b"\r")
        assert BACKSLASH not in result, f"Payload contains backslash: {result!r}"

    def test_crlf_does_not_produce_backslash(self) -> None:
        result = native_windows_terminal_input_bytes(b"\r\n")
        assert BACKSLASH not in result, f"Payload contains backslash: {result!r}"

    def test_text_with_newline_does_not_produce_backslash(self) -> None:
        result = native_windows_terminal_input_bytes(b"hello\nworld\n")
        assert BACKSLASH not in result, f"Payload contains backslash: {result!r}"

    def test_plain_text_unchanged(self) -> None:
        result = native_windows_terminal_input_bytes(b"hello")
        assert result == b"hello"

    def test_backslash_passthrough_is_not_duplicated(self) -> None:
        """If the user actually types a backslash it should pass through once."""
        result = native_windows_terminal_input_bytes(b"\\")
        assert result == b"\\"

    def test_backslash_before_newline_not_duplicated(self) -> None:
        """Backslash followed by newline — should not double the backslash."""
        result = native_windows_terminal_input_bytes(b"\\\n")
        # On Windows: \n → \r, so expect b"\\\r"
        # On non-Windows: passthrough, so b"\\\n"
        assert result.count(BACKSLASH) == 1, f"Unexpected backslash count: {result!r}"


# ── Integration tests: PTY write→read round-trip ─────────────────────────


@live
@skip_unless_github_actions
@skip_unless_dedicated_gh_pty_runner
class TestPtyTrailingBackslash(unittest.TestCase):
    """Ensure PTY write+read round-trip never introduces trailing backslashes."""

    @pytest.fixture(autouse=True)
    def _suppress_pty_text_warning(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setenv("RUNNING_PROCESS_NO_PTY_TEXT_WARNING", "1")

    def test_short_input_no_trailing_backslash(self) -> None:
        """Write a short line (well under 80 cols) and verify echo is clean."""
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
            cols=80,
        )
        try:
            _read_until_contains(process, "ready>")
            process.write("hello\n")
            output = _read_until_contains(process, "echo:hello")
            _assert_no_trailing_backslash(output, "short input")
        finally:
            process.close()

    def test_multiple_short_lines_no_trailing_backslash(self) -> None:
        """Program prints several lines — none should have trailing backslash."""
        process = RunningProcess.pseudo_terminal(
            [
                sys.executable,
                "-c",
                (
                    "print('line0:hello')\n"
                    "print('line1:world')\n"
                    "print('line2:done')\n"
                    "print('MARKER')\n"
                ),
            ],
            text=True,
            cols=120,
        )
        try:
            output = _read_until_contains(process, "MARKER")
            _assert_no_trailing_backslash(output, "multiple short lines")
        finally:
            process.close()

    def test_near_column_boundary_no_trailing_backslash(self) -> None:
        """Write input near the column boundary to test ConPTY wrapping."""
        cols = 40
        process = RunningProcess.pseudo_terminal(
            [
                sys.executable,
                "-c",
                (
                    "import sys\n"
                    "sys.stdout.write('ready>'); sys.stdout.flush()\n"
                    "line = sys.stdin.readline().strip()\n"
                    "sys.stdout.write(f'echo:{line}\\n'); sys.stdout.flush()\n"
                ),
            ],
            text=True,
            cols=cols,
        )
        try:
            _read_until_contains(process, "ready>")
            # "ready>" is 6 chars, typed text starts at col 7.
            # Send chars that stay within the column boundary.
            text = "x" * (cols - 7)
            process.write(f"{text}\n")
            output = _read_until_contains(process, f"echo:{text}")
            _assert_no_trailing_backslash(output, "near-boundary input")
        finally:
            process.close()

    def test_over_column_boundary_no_trailing_backslash(self) -> None:
        """Write input exceeding column width — ConPTY will wrap the echo.

        The raw output will contain \\r\\n at the wrap point.  We strip ANSI
        and check that no *visible* line ends with a literal backslash.
        """
        cols = 40
        # Use a program that prints a known marker so we can find the echo
        # even when ConPTY inserts line breaks inside it.
        process = RunningProcess.pseudo_terminal(
            [
                sys.executable,
                "-c",
                (
                    "import sys\n"
                    "sys.stdout.write('ready>'); sys.stdout.flush()\n"
                    "line = sys.stdin.readline().strip()\n"
                    "sys.stdout.write('END_ECHO\\n'); sys.stdout.flush()\n"
                ),
            ],
            text=True,
            cols=cols,
        )
        try:
            _read_until_contains(process, "ready>")
            text = "A" * (cols + 20)
            process.write(f"{text}\n")
            output = _read_until_contains(process, "END_ECHO")
            _assert_no_trailing_backslash(output, "over-boundary input")
        finally:
            process.close()

    def test_exact_column_boundary_no_trailing_backslash(self) -> None:
        """Write input that exactly fills the terminal width.

        This is the classic ConPTY edge case — output ending exactly at
        column N may trigger wrapping artefacts.
        """
        cols = 40
        process = RunningProcess.pseudo_terminal(
            [
                sys.executable,
                "-c",
                (
                    "import sys\n"
                    "line = sys.stdin.readline().strip()\n"
                    "sys.stdout.write('END_ECHO\\n'); sys.stdout.flush()\n"
                ),
            ],
            text=True,
            cols=cols,
        )
        try:
            time.sleep(0.3)
            text = "B" * cols
            process.write(f"{text}\n")
            output = _read_until_contains(process, "END_ECHO")
            _assert_no_trailing_backslash(output, "exact-boundary input")
        finally:
            process.close()

    def test_output_program_long_line_no_trailing_backslash(self) -> None:
        """A program printing a line longer than cols must not get backslashes.

        This isolates the *output* wrapping from input echo — the program
        itself emits a long line and ConPTY wraps it.
        """
        cols = 30
        long_text = "Z" * 60
        process = RunningProcess.pseudo_terminal(
            [
                sys.executable,
                "-c",
                f"print('{long_text}'); print('MARKER')",
            ],
            text=True,
            cols=cols,
        )
        try:
            output = _read_until_contains(process, "MARKER")
            _assert_no_trailing_backslash(output, "long output line")
        finally:
            process.close()
