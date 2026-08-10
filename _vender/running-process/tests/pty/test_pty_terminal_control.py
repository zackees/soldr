"""ANSI/VT/CSI escape preservation and redraw markers in PTY output."""

from __future__ import annotations

import io
import sys

import pytest

# `_safe_console_write_chunk` and the `_WINDOWS_VT_OUTPUT_HANDLES` cache
# moved to the `_console_io` sub-module in the #151 refactor; patch there
# so the production lookup site sees the change.
import running_process.pty._console_io as pty_module
from running_process import RunningProcess
from running_process.pty import PseudoTerminalProcess
from tests.pty._pty_helpers import _capture_wait_echo_bytes, _read_until_contains


def test_pseudo_terminal_wait_echo_preserves_ansi_color_sequences(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    fake_stdout = io.TextIOWrapper(io.BytesIO(), encoding="utf-8", newline="")
    monkeypatch.setattr(sys, "stdout", fake_stdout)
    process = RunningProcess(
        [
            sys.executable,
            "-c",
            ("import sys; sys.stdout.buffer.write(b'\\x1b[31mred\\x1b[0m'); sys.stdout.flush()"),
        ],
        use_pty=True,
        capture=True,
        text=True,
    )

    assert process.wait(timeout=5, echo=True) == 0
    fake_stdout.flush()

    echoed = fake_stdout.buffer.getvalue()
    assert echoed == process.stdout
    assert b"red" in echoed
    assert b"\x1b[" in echoed


def test_pseudo_terminal_wait_echo_enables_windows_vt_output_for_ansi_sequences(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    fake_stdout = io.TextIOWrapper(io.BytesIO(), encoding="utf-8", newline="")
    monkeypatch.setattr(sys, "stdout", fake_stdout)
    monkeypatch.setattr(pty_module.sys, "platform", "win32")
    monkeypatch.setattr(pty_module, "_WINDOWS_VT_OUTPUT_HANDLES", set())

    handles: list[int] = []

    def fake_windows_console_output_handle(stream: io.TextIOWrapper) -> int:
        return 77

    def fake_enable_windows_vt_output_handle(handle: int) -> bool:
        handles.append(handle)
        return True

    monkeypatch.setattr(
        pty_module,
        "_windows_console_output_handle",
        fake_windows_console_output_handle,
    )
    monkeypatch.setattr(
        pty_module,
        "_enable_windows_vt_output_handle",
        fake_enable_windows_vt_output_handle,
    )

    process = RunningProcess(
        [
            sys.executable,
            "-c",
            (
                "import sys; "
                "sys.stdout.buffer.write(b'\\x1b[?25l\\x1b[31mred\\x1b[0m\\x1b[?25h'); "
                "sys.stdout.flush()"
            ),
        ],
        use_pty=True,
        capture=True,
        text=True,
    )

    assert process.wait(timeout=5, echo=True) == 0
    fake_stdout.flush()

    echoed = fake_stdout.buffer.getvalue()
    assert handles == [77]
    assert echoed == process.stdout
    assert b"red" in echoed
    assert b"\x1b[" in echoed


def test_safe_console_write_chunk_enables_windows_vt_output_once(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(pty_module.sys, "platform", "win32")
    monkeypatch.setattr(pty_module, "_WINDOWS_VT_OUTPUT_HANDLES", set())

    handles: list[int] = []

    def fake_windows_console_output_handle(stream: io.TextIOWrapper) -> int:
        return 99

    def fake_enable_windows_vt_output_handle(handle: int) -> bool:
        handles.append(handle)
        return True

    monkeypatch.setattr(
        pty_module,
        "_windows_console_output_handle",
        fake_windows_console_output_handle,
    )
    monkeypatch.setattr(
        pty_module,
        "_enable_windows_vt_output_handle",
        fake_enable_windows_vt_output_handle,
    )

    fake_stdout = io.TextIOWrapper(io.BytesIO(), encoding="utf-8", newline="")

    pty_module._safe_console_write_chunk(
        fake_stdout,
        b"\x1b[?25l",
        encoding="utf-8",
        errors="replace",
    )
    pty_module._safe_console_write_chunk(
        fake_stdout,
        b"\x1b[?25h",
        encoding="utf-8",
        errors="replace",
    )

    fake_stdout.flush()
    assert handles == [99]
    assert fake_stdout.buffer.getvalue() == b"\x1b[?25l\x1b[?25h"


def test_pseudo_terminal_wait_echo_preserves_carriage_return_redraws(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    fake_stdout = io.TextIOWrapper(io.BytesIO(), encoding="utf-8", newline="")
    monkeypatch.setattr(sys, "stdout", fake_stdout)
    process = RunningProcess(
        [
            sys.executable,
            "-c",
            (
                "import sys, time; "
                "sys.stdout.write('first'); sys.stdout.flush(); "
                "time.sleep(0.05); "
                "sys.stdout.write('\\rsecond'); sys.stdout.flush()"
            ),
        ],
        use_pty=True,
        capture=True,
        text=True,
    )

    assert process.wait(timeout=5, echo=True) == 0
    fake_stdout.flush()

    echoed = fake_stdout.buffer.getvalue()
    assert echoed == process.stdout
    assert b"first" in echoed
    assert b"second" in echoed
    assert any(marker in echoed for marker in (b"\r", b"\x1b[H", b"\x1b[1G"))


def test_pseudo_terminal_wait_echo_preserves_progress_redraw_sequences() -> None:
    process = RunningProcess(
        [
            sys.executable,
            "-c",
            (
                "import sys, time\n"
                "for value in (10, 20, 30):\n"
                "    sys.stdout.write(f'progress {value}%\\r')\n"
                "    sys.stdout.flush()\n"
                "    time.sleep(0.02)\n"
                "sys.stdout.write('done\\n')\n"
                "sys.stdout.flush()\n"
            ),
        ],
        use_pty=True,
        capture=True,
        text=True,
    )

    echoed = _capture_wait_echo_bytes(process)

    assert echoed == process.stdout
    assert b"progress 10%" in echoed
    assert b"progress 20%" in echoed
    assert b"progress 30%" in echoed
    assert b"done\r\n" in echoed
    assert any(marker in echoed for marker in (b"\r", b"\x1b[H", b"\x1b[1G"))


def test_pseudo_terminal_progress_capture_preserves_redraw_markers() -> None:
    process = RunningProcess(
        [
            sys.executable,
            "-c",
            (
                "import sys, time\n"
                "for value in (10, 20, 30):\n"
                "    sys.stdout.write(f'progress {value}%\\r')\n"
                "    sys.stdout.flush()\n"
                "    time.sleep(0.02)\n"
                "sys.stdout.write('done\\n')\n"
                "sys.stdout.flush()\n"
            ),
        ],
        use_pty=True,
        capture=True,
        text=True,
    )

    assert process.wait(timeout=5) == 0
    assert isinstance(process.stdout, bytes)
    assert b"progress 10%" in process.stdout
    assert b"progress 20%" in process.stdout
    assert b"progress 30%" in process.stdout
    assert b"done\r\n" in process.stdout
    assert any(marker in process.stdout for marker in (b"\r", b"\x1b[H", b"\x1b[1G"))


def test_pseudo_terminal_internal_chunk_capture_keeps_cursor_home_bytes_untouched() -> None:
    process = PseudoTerminalProcess([sys.executable, "-c", "print('x')"], text=True, auto_run=False)

    process._handle_native_chunk(b"first")
    process._handle_native_chunk(b"\x1b[25l\x1b[Hsecond\x1b[?25h")

    assert process.output == b"first\x1b[25l\x1b[Hsecond\x1b[?25h"
    assert process.drain_echo() == [b"first", b"\x1b[25l\x1b[Hsecond\x1b[?25h"]


def test_pseudo_terminal_internal_chunk_capture_keeps_sgr_bytes_untouched() -> None:
    process = PseudoTerminalProcess([sys.executable, "-c", "print('x')"], text=True, auto_run=False)

    process._handle_native_chunk(b"\x1b[31mred\x1b[0m")

    assert process.output == b"\x1b[31mred\x1b[0m"
    assert process.drain_echo() == [b"\x1b[31mred\x1b[0m"]


def test_pseudo_terminal_internal_chunk_capture_keeps_query_and_title_bytes_untouched() -> None:
    process = PseudoTerminalProcess([sys.executable, "-c", "print('x')"], text=True, auto_run=False)

    process._handle_native_chunk(b"\x1b[6n\x1b]0;python\x07visible")

    assert process.output == b"\x1b[6n\x1b]0;python\x07visible"
    assert process.drain_echo() == [b"\x1b[6n\x1b]0;python\x07visible"]


def test_pseudo_terminal_internal_chunk_capture_keeps_cursor_motion_bytes_untouched() -> None:
    process = PseudoTerminalProcess([sys.executable, "-c", "print('x')"], text=True, auto_run=False)

    process._handle_native_chunk(b"\x1b[2Aup\x1b[1Bdown\x1b[2K")

    assert process.output == b"\x1b[2Aup\x1b[1Bdown\x1b[2K"
    assert process.drain_echo() == [b"\x1b[2Aup\x1b[1Bdown\x1b[2K"]


def test_pseudo_terminal_keeps_control_sequences_from_subprocess_output_untouched() -> None:
    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            (
                "import sys, time; "
                "sys.stdout.buffer.write(b'prefix'); sys.stdout.flush(); "
                "payload = ("
                "    b'\\x1b[0;0;27;1;0;1_'"
                "    b'\\x1b[0;0;91;1;0;1_'"
                "    b'\\x1b[0;0;49;1;0;1_'"
                "    b'\\x1b[0;0;51;1;0;1_'"
                "    b'\\x1b[0;0;59;1;0;1_'"
                "    b'\\x1b[0;0;50;1;0;1_'"
                "    b'\\x1b[0;0;56;1;0;1_'"
                "    b'\\x1b[0;0;59;1;0;1_'"
                "    b'\\x1b[0;0;49;1;0;1_'"
                "    b'\\x1b[0;0;51;1;0;1_'"
                "    b'\\x1b[0;0;59;1;0;1_'"
                "    b'\\x1b[0;0;48;1;0;1_'"
                "    b'\\x1b[0;0;59;1;0;1_'"
                "    b'\\x1b[0;0;51;1;0;1_'"
                "    b'\\x1b[0;0;50;1;0;1_'"
                "    b'\\x1b[0;0;59;1;0;1_'"
                "    b'\\x1b[0;0;49;1;0;1_'"
                "); "
                "sys.stdout.buffer.write(payload[:24]); sys.stdout.flush(); "
                "time.sleep(0.05); "
                "sys.stdout.buffer.write(payload[24:]); sys.stdout.flush(); "
                "time.sleep(0.05); "
                "sys.stdout.buffer.write(b'\\x1b[?2004l\\x1b[?1049lvisible\\r\\n'); "
                "sys.stdout.flush()"
            ),
        ],
        text=True,
    )

    output = _read_until_contains(process, "visible")
    assert "prefix" in output
    assert "visible" in output
    assert process.wait(timeout=5) == 0
    assert isinstance(process.output, bytes)
    assert b"\x1b" in process.output
    assert b"0;0;27;1;0;1_" in process.output
    assert b"?2004l" in process.output
