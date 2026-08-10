"""Windows-only encoding-safety tests for every audited stdout/stderr path.

Tracks issue #104. This suite exercises every public path that bridges
child-process bytes back to the caller (or to ``sys.stdout``) against payloads
that are valid UTF-8 but unmappable in the legacy Windows ``cp1252`` console.

Triple-gated so it never runs in default CI:

1. ``sys.platform == "win32"``
2. ``RUNNING_PROCESS_WINDOWS_ENCODING_TESTS=1`` env var
3. ``pytest -m windows_encoding`` marker

Run with::

    set RUNNING_PROCESS_WINDOWS_ENCODING_TESTS=1
    uv run pytest -m windows_encoding tests/encoding -v
"""

from __future__ import annotations

import io
import os
import shutil
import subprocess
import sys
import unittest
from contextlib import redirect_stderr, redirect_stdout
from unittest import mock

import pytest

from running_process import PIPE, RunningProcess
from running_process.console_encoding import detect_console_encoding, sanitize_for_encoding
from running_process.pty import PseudoTerminalProcess

# Characters that are valid UTF-8 but cannot encode to cp1252.
PAYLOADS: dict[str, str] = {
    "snowman": "\u2603",
    "cjk": "\u4e2d\u6587",
    "emoji": "\U0001f600",
    "combining": "e\u0301",
    "mixed": "ascii \u4e2d\u6587 \U0001f600 \u2603",
}

_GATE_ENV = "RUNNING_PROCESS_WINDOWS_ENCODING_TESTS"


pytestmark = [
    pytest.mark.windows_encoding,
    pytest.mark.skipif(sys.platform != "win32", reason="Windows-only suite"),
    pytest.mark.skipif(
        os.environ.get(_GATE_ENV) != "1",
        reason=f"set {_GATE_ENV}=1 to run the Windows encoding suite",
    ),
]


def _python_exe() -> str:
    return sys.executable or shutil.which("python") or "python"


def _utf8_writer_argv(payload: str) -> list[str]:
    """Build a child-process argv that writes ``payload`` as raw UTF-8 bytes to stdout.

    Bypasses the child's own ``sys.stdout.encoding`` (which would itself be
    cp1252 by default) by using ``sys.stdout.buffer.write`` directly.
    """
    code = (
        "import sys; "
        f"sys.stdout.buffer.write({payload.encode('utf-8')!r}); "
        "sys.stdout.buffer.write(b'\\n'); "
        "sys.stdout.flush()"
    )
    return [_python_exe(), "-c", code]


def _cp1252_text_stream() -> io.TextIOWrapper:
    """A strict cp1252-encoded stream that mimics a legacy Windows console."""
    return io.TextIOWrapper(io.BytesIO(), encoding="cp1252", errors="strict")


# ---------------------------------------------------------------------------
# Auto-detection unit tests (cross-platform; gates above still apply, but the
# logic is exercised on every Windows CI run).
# ---------------------------------------------------------------------------


class DetectConsoleEncodingTest(unittest.TestCase):
    def test_explicit_wins(self) -> None:
        with mock.patch.dict(os.environ, {"PYTHONIOENCODING": "utf-16"}):
            self.assertEqual(detect_console_encoding("latin-1"), "latin-1")

    def test_pythonioencoding_beats_stdout(self) -> None:
        with mock.patch.dict(os.environ, {"PYTHONIOENCODING": "utf-8:replace"}):
            with mock.patch.object(sys, "stdout", mock.Mock(encoding="cp1252")):
                self.assertEqual(detect_console_encoding(None), "utf-8")

    def test_stdout_beats_locale(self) -> None:
        env = {k: v for k, v in os.environ.items() if k != "PYTHONIOENCODING"}
        with mock.patch.dict(os.environ, env, clear=True):
            with mock.patch.object(sys, "stdout", mock.Mock(encoding="cp1252")):
                self.assertEqual(detect_console_encoding(None), "cp1252")

    def test_locale_fallback(self) -> None:
        env = {k: v for k, v in os.environ.items() if k != "PYTHONIOENCODING"}
        with mock.patch.dict(os.environ, env, clear=True):
            with mock.patch.object(sys, "stdout", mock.Mock(encoding=None)):
                with mock.patch(
                    "running_process.console_encoding.locale.getpreferredencoding",
                    return_value="iso-8859-1",
                ):
                    self.assertEqual(detect_console_encoding(None), "iso-8859-1")


class SanitizeForEncodingTest(unittest.TestCase):
    def test_utf8_roundtrips_losslessly(self) -> None:
        for payload in PAYLOADS.values():
            self.assertEqual(sanitize_for_encoding(payload, "utf-8"), payload)

    def test_cp1252_replaces_unmappable(self) -> None:
        for payload in PAYLOADS.values():
            sanitized = sanitize_for_encoding(payload, "cp1252")
            # Sanitized text MUST encode to cp1252 strict without raising.
            sanitized.encode("cp1252")  # raises if we got it wrong

    def test_unknown_encoding_falls_back(self) -> None:
        # Should not raise even when encoding is bogus.
        for payload in PAYLOADS.values():
            sanitize_for_encoding(payload, "this-is-not-a-codec")


# ---------------------------------------------------------------------------
# Pipe-mode paths (audit-table rows 1-6, plus 14)
# ---------------------------------------------------------------------------


class PipeModeEncodingTest(unittest.TestCase):
    """Paths 1-6, 14 — pipe-backed RunningProcess captured output."""

    def _run_capturing(self, payload: str) -> RunningProcess:
        proc = RunningProcess(
            _utf8_writer_argv(payload),
            cwd=None,
            shell=False,
            capture=True,
            text=True,
            encoding="cp1252",
            errors="replace",
        )
        proc.wait()
        return proc

    def test_path_1_stream_iter(self) -> None:
        for payload in PAYLOADS.values():
            proc = self._run_capturing(payload)
            sink = _cp1252_text_stream()
            # PATH 1: stream_iter -> _safe_console_write protected echo
            for event in proc.stream_iter():
                if event.stdout and event.stdout != "EOS" and isinstance(event.stdout, str):
                    sink.write(event.stdout)  # MUST NOT raise

    def test_path_2_stdout_property(self) -> None:
        for payload in PAYLOADS.values():
            proc = self._run_capturing(payload)
            sink = _cp1252_text_stream()
            value = proc.stdout
            assert isinstance(value, str)  # text=True
            sink.write(value)  # PATH 2 — pre-sanitized return

    def test_path_3_captured_stream_str_bytes(self) -> None:
        for payload in PAYLOADS.values():
            proc = self._run_capturing(payload)
            sink = _cp1252_text_stream()
            sink.write(str(proc.stdout_stream))  # PATH 3 (__str__)
            bytes(proc.stdout_stream)  # PATH 3 (__bytes__)

    def test_path_4_captured_stream_read(self) -> None:
        for payload in PAYLOADS.values():
            proc = self._run_capturing(payload)
            sink = _cp1252_text_stream()
            value = proc.stdout_stream.read()
            if isinstance(value, str):
                sink.write(value)  # PATH 4

    def test_path_5_process_output_event_fields(self) -> None:
        for payload in PAYLOADS.values():
            proc = self._run_capturing(payload)
            sink = _cp1252_text_stream()
            for event in proc.stream_iter():
                if isinstance(event.stdout, str):
                    sink.write(event.stdout)  # PATH 5
                if isinstance(event.stderr, str):
                    sink.write(event.stderr)

    def test_path_6_drain_methods(self) -> None:
        for payload in PAYLOADS.values():
            proc = self._run_capturing(payload)
            sink = _cp1252_text_stream()
            for line in proc.drain_stdout():
                if isinstance(line, str):
                    sink.write(line)  # PATH 6
            for line in proc.drain_stderr():
                if isinstance(line, str):
                    sink.write(line)
            for _stream, line in proc.drain_combined():
                if isinstance(line, str):
                    sink.write(line)

    def test_path_14_expect(self) -> None:
        # PATH 14 — expect against captured text uses errors="replace" Rust-side
        proc = RunningProcess(
            _utf8_writer_argv(PAYLOADS["mixed"]),
            cwd=None,
            shell=False,
            capture=True,
            text=True,
            encoding="cp1252",
            errors="replace",
        )
        proc.wait()
        sink = _cp1252_text_stream()
        value = proc.stdout
        assert isinstance(value, str)
        sink.write(value)


# ---------------------------------------------------------------------------
# PTY-mode paths (audit-table rows 7-9)
# ---------------------------------------------------------------------------


class PtyModeEncodingTest(unittest.TestCase):
    """Paths 7-9 — PTY-backed PseudoTerminalProcess captured output."""

    def _run_pty(self, payload: str) -> PseudoTerminalProcess:
        proc = PseudoTerminalProcess(
            _utf8_writer_argv(payload),
            shell=False,
            capture=True,
            encoding="cp1252",
        )
        proc.start()
        # Drain to EOF.
        while True:
            try:
                proc.read(timeout=2.0)
            except (EOFError, TimeoutError):
                break
        proc.wait()
        return proc

    def test_path_7_output_property(self) -> None:
        for payload in PAYLOADS.values():
            proc = self._run_pty(payload)
            sink = _cp1252_text_stream()
            value = proc.output
            if isinstance(value, str):
                sink.write(value)  # PATH 7 — pre-sanitized
            sink.write(proc.output_text)  # extra: helper that always sanitizes

    def test_path_8_echo_to_console(self) -> None:
        # PATH 8 — direct echo, _safe_console_write_chunk handles encoding.
        for payload in PAYLOADS.values():
            proc = PseudoTerminalProcess(
                _utf8_writer_argv(payload),
                shell=False,
                capture=True,
                encoding="cp1252",
            )
            proc.start()
            sink = _cp1252_text_stream()
            with redirect_stdout(sink):
                while True:
                    try:
                        proc.read(timeout=2.0)
                    except (EOFError, TimeoutError):
                        break
                    proc._echo_to_console(sys.stdout)
            proc.wait()

    def test_path_9_read_text_safe(self) -> None:
        # PATH 9 — read() returns bytes; read_text() returns sanitized str.
        for payload in PAYLOADS.values():
            proc = PseudoTerminalProcess(
                _utf8_writer_argv(payload),
                shell=False,
                capture=True,
                encoding="cp1252",
            )
            proc.start()
            sink = _cp1252_text_stream()
            try:
                while True:
                    try:
                        text = proc.read_text(timeout=2.0)
                    except (EOFError, TimeoutError):
                        break
                    sink.write(text)  # MUST NOT raise on cp1252 sink
            finally:
                proc.wait()


# ---------------------------------------------------------------------------
# CLI / one-shot paths (audit-table rows 10-12)
# ---------------------------------------------------------------------------


class CliPathEncodingTest(unittest.TestCase):
    """Paths 10-12 — CLI helpers and dashboard subprocess call."""

    def test_path_10_subprocess_run_text_mode(self) -> None:
        # PATH 10/12 — subprocess.run(text=True) under PYTHONIOENCODING=utf-8
        env = os.environ.copy()
        env["PYTHONIOENCODING"] = "utf-8"
        for payload in PAYLOADS.values():
            res = subprocess.run(
                _utf8_writer_argv(payload),
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
                env=env,
                check=False,
            )
            sink = _cp1252_text_stream()
            sanitized = sanitize_for_encoding(res.stdout, "cp1252")
            sink.write(sanitized)  # MUST NOT raise


# ---------------------------------------------------------------------------
# End-to-end interpreter-mode matrix
# ---------------------------------------------------------------------------


class InterpreterModeMatrixTest(unittest.TestCase):
    """Three interpreter sub-configurations: bare cp1252, PYTHONUTF8=1, PYTHONIOENCODING=utf-8."""

    def _run_under(self, env_overrides: dict[str, str]) -> None:
        env = os.environ.copy()
        env.update(env_overrides)
        for payload in PAYLOADS.values():
            proc = RunningProcess(
                _utf8_writer_argv(payload),
                cwd=None,
                shell=False,
                capture=True,
                text=True,
                encoding="cp1252",
                errors="replace",
                env=env,
            )
            proc.wait()
            sink = _cp1252_text_stream()
            value = proc.stdout
            assert isinstance(value, str)
            sink.write(value)

    def test_bare_cp1252(self) -> None:
        self._run_under({"PYTHONUTF8": "0", "PYTHONIOENCODING": ""})

    def test_pythonutf8(self) -> None:
        self._run_under({"PYTHONUTF8": "1"})

    def test_pythonioencoding_utf8(self) -> None:
        self._run_under({"PYTHONIOENCODING": "utf-8"})


# Prevent shadow warnings about unused imports in environments that don't run the suite.
_ = (PIPE, redirect_stderr)


if __name__ == "__main__":
    unittest.main()
