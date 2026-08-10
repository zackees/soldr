"""Pseudo-terminal write/input-relay helpers.

Free-function bodies extracted from ``PseudoTerminalProcess``. Lookups
for ``NativeTerminalInput`` and ``sys`` are deferred to the
``_pseudo_terminal`` module so tests can monkey-patch
``running_process.pty._pseudo_terminal.NativeTerminalInput`` and
``running_process.pty._pseudo_terminal.sys.platform`` and have the
patches take effect here.
"""

from __future__ import annotations

import os
import threading
import time
from contextlib import suppress
from typing import TYPE_CHECKING, Any

from running_process.pty._idle_helpers import _input_contains_newline

if TYPE_CHECKING:
    from running_process.pty._pseudo_terminal import PseudoTerminalProcess


def write(
    process: PseudoTerminalProcess,
    data: str | bytes,
    *,
    submit: bool = False,
) -> None:
    process._ensure_started()
    raw = (
        data.encode(process.encoding, process.errors) if isinstance(data, str) else data
    )
    process._pty_input_bytes_total += len(raw)
    if _input_contains_newline(raw):
        process._pty_newline_events_total += 1
    if submit:
        process._pty_submit_events_total += 1
    process.last_activity_at = time.time()
    if process._native_idle_detector is not None:
        process._native_idle_detector.record_input(len(raw))
    assert process._proc is not None
    process._proc.write(raw, submit=submit)
    sync_native_input_metrics(process)


def submit(process: PseudoTerminalProcess, data: str | bytes = "\n") -> None:
    process.write(data, submit=True)


def terminal_input_relay_active(process: PseudoTerminalProcess) -> bool:
    from running_process.pty import _pseudo_terminal as _pt

    if (
        _pt.sys.platform == "win32"
        and process._proc is not None
        and hasattr(process._proc, "terminal_input_relay_active")
    ):
        active = bool(process._proc.terminal_input_relay_active())
        sync_native_input_metrics(process)
        return active
    thread = process._terminal_input_thread
    return thread is not None and thread.is_alive()


def sync_native_input_metrics(process: PseudoTerminalProcess) -> None:
    if process._proc is None or not hasattr(process._proc, "pty_input_bytes_total"):
        return
    # Only sync when we need to detect submit events for idle timeout arming.
    if not process._arm_idle_timeout_on_submit or process.idle_timeout_enabled:
        return
    input_bytes_total = int(process._proc.pty_input_bytes_total())
    newline_events_total = int(process._proc.pty_newline_events_total())
    submit_events_total = int(process._proc.pty_submit_events_total())
    submit_delta = submit_events_total - process._pty_submit_events_total
    process._pty_input_bytes_total = input_bytes_total
    process._pty_newline_events_total = newline_events_total
    process._pty_submit_events_total = submit_events_total
    if submit_delta > 0:
        process.idle_timeout_enabled = True


def maybe_arm_idle_timeout_from_terminal_input(
    process: PseudoTerminalProcess,
    *,
    submit: bool,
) -> None:
    if not process._arm_idle_timeout_on_submit and not submit:
        return
    if not submit or process.idle_timeout_enabled:
        return
    process.idle_timeout_enabled = True


def start_windows_terminal_input_relay(process: PseudoTerminalProcess) -> None:
    from running_process.pty import _pseudo_terminal as _pt

    if (
        process._allows_child_ctrl_c_interruption
        and process._proc is not None
        and hasattr(process._proc, "start_terminal_input_relay")
    ):
        process._proc.start_terminal_input_relay()
        sync_native_input_metrics(process)
        return
    capture = _pt.NativeTerminalInput()
    capture.start()
    process._terminal_input_capture = capture
    filter_ctrl_c = not process._allows_child_ctrl_c_interruption

    def relay() -> None:
        try:
            while not process._terminal_input_stop.is_set() and process.poll() is None:
                try:
                    data, submit_flag = capture.read_batch(timeout=0.05)
                except TimeoutError:
                    continue
                if filter_ctrl_c:
                    data = data.replace(b"\x03", b"")
                    if not data:
                        continue
                maybe_arm_idle_timeout_from_terminal_input(process, submit=submit_flag)
                process.write(data, submit=submit_flag)
        finally:
            with suppress(Exception):
                capture.close()

    process._terminal_input_thread = threading.Thread(
        target=relay,
        daemon=True,
        name=f"pty-terminal-input-{process.pid or 'pending'}",
    )
    process._terminal_input_thread.start()


def start_posix_terminal_input_relay(process: PseudoTerminalProcess) -> None:
    import select
    import termios
    import tty

    from running_process.pty import _pseudo_terminal as _pt

    if not _pt.sys.stdin.isatty():
        return

    stdin_fd = _pt.sys.stdin.fileno()
    previous_state = termios.tcgetattr(stdin_fd)
    tty.setraw(stdin_fd)
    process._terminal_input_restore_state = (stdin_fd, previous_state)
    filter_ctrl_c = not process._allows_child_ctrl_c_interruption

    def relay() -> None:
        try:
            while not process._terminal_input_stop.is_set() and process.poll() is None:
                try:
                    ready, _, _ = select.select([stdin_fd], [], [], 0.05)
                except (OSError, ValueError):
                    return
                if not ready:
                    continue
                data = os.read(stdin_fd, 65536)
                if not data:
                    continue
                # Drain any additional data already in the fd buffer
                # so large pastes arrive as a single write.
                while True:
                    try:
                        more_ready, _, _ = select.select([stdin_fd], [], [], 0)
                    except (OSError, ValueError):
                        break
                    if not more_ready:
                        break
                    more = os.read(stdin_fd, 65536)
                    if not more:
                        break
                    data += more
                if filter_ctrl_c:
                    data = data.replace(b"\x03", b"")
                    if not data:
                        continue
                submit_flag = b"\r" in data or b"\n" in data
                maybe_arm_idle_timeout_from_terminal_input(process, submit=submit_flag)
                process.write(data, submit=submit_flag)
        finally:
            restore_posix_terminal_input(process)

    process._terminal_input_thread = threading.Thread(
        target=relay,
        daemon=True,
        name=f"pty-terminal-input-{process.pid or 'pending'}",
    )
    process._terminal_input_thread.start()


def restore_posix_terminal_input(process: PseudoTerminalProcess) -> None:
    state: Any = process._terminal_input_restore_state
    if state is None:
        return
    process._terminal_input_restore_state = None
    import termios

    stdin_fd, previous_state = state
    with suppress(Exception):
        termios.tcsetattr(stdin_fd, termios.TCSANOW, previous_state)


def start_terminal_input_relay(
    process: PseudoTerminalProcess,
    *,
    arm_idle_timeout_on_submit: bool | None = None,
) -> None:
    from running_process.pty import _pseudo_terminal as _pt

    process._ensure_started()
    if process.terminal_input_relay_active:
        return
    if arm_idle_timeout_on_submit is not None:
        process._arm_idle_timeout_on_submit = bool(arm_idle_timeout_on_submit)
    process._terminal_input_stop = threading.Event()
    if _pt.sys.platform == "win32":
        start_windows_terminal_input_relay(process)
        return
    start_posix_terminal_input_relay(process)


def stop_terminal_input_relay(process: PseudoTerminalProcess) -> None:
    from running_process.pty import _pseudo_terminal as _pt

    process._terminal_input_stop.set()
    if (
        _pt.sys.platform == "win32"
        and process._proc is not None
        and hasattr(process._proc, "stop_terminal_input_relay")
    ):
        with suppress(Exception):
            process._proc.stop_terminal_input_relay()
        sync_native_input_metrics(process)
    thread = process._terminal_input_thread
    if thread is not None and thread is not threading.current_thread():
        thread.join(timeout=0.2)
    process._terminal_input_thread = None
    capture = process._terminal_input_capture
    process._terminal_input_capture = None
    if capture is not None:
        with suppress(Exception):
            capture.close()
    restore_posix_terminal_input(process)
