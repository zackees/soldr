from __future__ import annotations

import os
import signal
import sys
import threading
import time
import weakref
from collections.abc import Callable, Mapping
from contextlib import suppress
from io import TextIOBase
from pathlib import Path
from typing import Any

from running_process._native import (
    NativeIdleDetector,
    NativeProcess,
    NativeProcessMetrics,
    NativePtyBuffer,
    NativeTerminalInput,
)
from running_process.console_encoding import (
    detect_console_encoding,
    sanitize_for_encoding,
)
from running_process.exit_status import (
    ExitStatus,
    ProcessAbnormalExit,
    classify_exit_status,
)
from running_process.expect import ExpectAction, ExpectMatch, ExpectPattern, ensure_text
from running_process.priority import CpuPriority, normalize_nice
from running_process.pty import (
    _pty_expect,
    _pty_idle_waiter,
    _pty_input_relay,
    _pty_reader,
    _pty_wait_for,
)
from running_process.pty._command import (
    _normalize_command,
    _pty_command,
    interactive_launch_spec,
)
from running_process.pty._console_io import _safe_console_write_chunk
from running_process.pty._errors import PtyNotAvailableError, SignalBool
from running_process.pty._idle_state import _IdleSample
from running_process.pty._process_helpers import (
    _close_native_pty_process,
    _warn_pty_text_mode_ignored,
)
from running_process.pty._types import (
    Expect,
    IdleDetection,
    IdleDetector,
    IdleWaitResult,
    InteractiveMode,
    InterruptResult,
    ProcessIdleDetection,
    WaitCheckpoint,
    WaitCondition,
    WaitForResult,
)

_SUPPORTED_PTY_PLATFORMS = {"win32", "linux", "darwin"}
_PTY_READ_CHUNK_TIMEOUT_SECONDS = 0.01
_PTY_POLL_INTERVAL_SECONDS = 0.001
_PTY_READER_NATIVE_CLOSE_WAIT_SECONDS = 2.0
_PTY_CLEANUP_ERRORS = (OSError, RuntimeError, TimeoutError, ValueError, AttributeError)

KEYBOARD_INTERRUPT_EXIT_CODES: set[int] = {
    -2,  # Unix: killed by SIGINT (negative signal number)
    130,  # Unix: 128 + SIGINT(2) — shell convention
    -1073741510,  # Windows: STATUS_CONTROL_C_EXIT (signed)
    3221225786,  # Windows: STATUS_CONTROL_C_EXIT (unsigned)
}


class Pty:
    @classmethod
    def is_available(cls) -> bool:
        return sys.platform in _SUPPORTED_PTY_PLATFORMS


class PseudoTerminalProcess:
    def __init__(
        self,
        command: str | list[str],
        *,
        cwd: str | Path | None = None,
        shell: bool | None = None,
        env: Mapping[str, str] | None = None,
        text: bool = False,
        encoding: str | None = None,
        errors: str = "replace",
        rows: int = 24,
        cols: int = 80,
        nice: int | CpuPriority | None = None,
        capture: bool = True,
        restore_terminal: bool = True,
        expect: list[Expect] | None = None,
        idle_detector: IdleDetector | None = None,
        relay_terminal_input: bool = False,
        arm_idle_timeout_on_submit: bool = False,
        allows_child_ctrl_c_interruption: bool = True,
        auto_run: bool = True,
    ) -> None:
        if not Pty.is_available():
            raise PtyNotAvailableError(
                f"Pseudo-terminal support is not available on unsupported platform: {sys.platform}"
            )
        command, shell = _normalize_command(command, shell)

        if text:
            _warn_pty_text_mode_ignored(env)
        self.command = command
        self.shell = shell
        self.cwd = str(cwd) if cwd is not None else None
        self.env = dict(env) if env is not None else os.environ.copy()
        self.text = False
        self.encoding = detect_console_encoding(encoding)
        self.errors = errors
        self.rows = rows
        self.cols = cols
        self.nice = normalize_nice(nice)
        self.capture = bool(capture)
        self.launch_spec = interactive_launch_spec(InteractiveMode.PSEUDO_TERMINAL)
        self.restore_terminal = restore_terminal

        self._proc: NativeProcess | None = None
        self._buffer = (
            NativePtyBuffer(text=False, encoding=self.encoding, errors=self.errors)
            if self.capture
            else None
        )
        self._native_stream_closed = False
        self._start_time: float | None = None
        self._end_time: float | None = None
        self._restored = False
        self._finalized = False
        self.exit_reason: str | None = None
        self.interrupt_count = 0
        self.interrupted_by_caller = False
        self.last_activity_at: float | None = None
        self._exit_status: ExitStatus | None = None
        self._pty_input_bytes_total = 0
        self._pty_newline_events_total = 0
        self._pty_output_bytes_total = 0
        self._pty_control_churn_bytes_total = 0
        self._pty_submit_events_total = 0
        self._pending_echo_chunks: list[bytes] = []
        self._native_idle_detector: NativeIdleDetector | None = None
        self._native_process_metrics: NativeProcessMetrics | None = None
        self._native_exit_watcher: threading.Thread | None = None
        self._close_finalizer: weakref.finalize | None = None
        self._idle_timeout_signal = SignalBool(True)
        self._registered_expect_conditions = list(expect) if expect is not None else []
        self._registered_idle_detector = idle_detector
        self._relay_terminal_input = bool(relay_terminal_input)
        self._arm_idle_timeout_on_submit = bool(arm_idle_timeout_on_submit)
        self._allows_child_ctrl_c_interruption = bool(allows_child_ctrl_c_interruption)
        self._terminal_input_capture: NativeTerminalInput | None = None
        self._terminal_input_thread: threading.Thread | None = None
        self._terminal_input_stop = threading.Event()
        self._terminal_input_restore_state: Any | None = None
        if auto_run:
            self.start()

    def start(self) -> None:
        if self._proc is not None:
            raise RuntimeError("Pseudo-terminal process already started")

        argv = _pty_command(self.command, self.shell, self.nice)
        self._proc = NativeProcess.for_pty(
            argv,
            cwd=self.cwd,
            env=self.env,
            rows=self.rows,
            cols=self.cols,
            nice=self.nice,
        )
        self._proc.start()

        self._start_time = time.time()
        self.last_activity_at = self._start_time
        if self.pid is not None:
            self._native_process_metrics = NativeProcessMetrics(self.pid)
        self._prime_process_metrics()
        self._close_finalizer = weakref.finalize(
            self, _close_native_pty_process, self._proc
        )
        self._native_stream_closed = False
        if self._relay_terminal_input:
            self.start_terminal_input_relay(
                arm_idle_timeout_on_submit=self._arm_idle_timeout_on_submit
            )

    def available(self) -> bool:
        if not self.capture:
            self._pump_native_output(timeout=0.0, consume_all=True)
            return False
        self._pump_native_output(timeout=0.0, consume_all=True)
        return self._buffer.available()

    @property
    def idle_timeout_enabled(self) -> bool:
        return self._idle_timeout_signal.value

    @idle_timeout_enabled.setter
    def idle_timeout_enabled(self, enabled: bool) -> None:
        enabled = bool(enabled)
        detector = self._native_idle_detector
        if detector is not None:
            detector.enabled = enabled
        self._idle_timeout_signal.value = enabled

    def read(self, timeout: float | None = None) -> str | bytes:
        return _pty_reader.read(self, timeout=timeout)

    def read_non_blocking(self) -> str | bytes | None:
        return _pty_reader.read_non_blocking(self)

    def read_text(self, timeout: float | None = None) -> str:
        """Like ``read()`` but always returns ``str``, decoded and sanitized for the parent console.

        Use this when the result will be printed to ``sys.stdout``: the value is
        round-tripped through the auto-detected console encoding with
        ``errors='replace'``, so writing it to a cp1252 console will not raise
        ``UnicodeEncodeError`` even when the child emitted UTF-8.
        """
        return _pty_reader.read_text(self, timeout=timeout)

    def drain(self) -> list[str | bytes]:
        return _pty_reader.drain(self)

    def drain_echo(self) -> list[bytes]:
        return _pty_reader.drain_echo(self)

    def discard_output(self) -> int:
        return _pty_reader.discard_output(self)

    @property
    def output_bytes(self) -> int:
        return _pty_reader.output_bytes(self)

    def _output_since(self, start: int) -> str | bytes:
        return _pty_reader.output_since(self, start)

    def _snapshot_output_history(self) -> tuple[str, int]:
        return _pty_reader.snapshot_output_history(self)

    def _snapshot_output_since(self, start: int) -> tuple[str, int]:
        return _pty_reader.snapshot_output_since(self, start)

    def write(self, data: str | bytes, *, submit: bool = False) -> None:
        _pty_input_relay.write(self, data, submit=submit)

    def submit(self, data: str | bytes = "\n") -> None:
        _pty_input_relay.submit(self, data)

    @property
    def terminal_input_relay_active(self) -> bool:
        return _pty_input_relay.terminal_input_relay_active(self)

    def _sync_native_input_metrics(self) -> None:
        _pty_input_relay.sync_native_input_metrics(self)

    def _maybe_arm_idle_timeout_from_terminal_input(self, *, submit: bool) -> None:
        _pty_input_relay.maybe_arm_idle_timeout_from_terminal_input(self, submit=submit)

    def _start_windows_terminal_input_relay(self) -> None:
        _pty_input_relay.start_windows_terminal_input_relay(self)

    def _start_posix_terminal_input_relay(self) -> None:
        _pty_input_relay.start_posix_terminal_input_relay(self)

    def _restore_posix_terminal_input(self) -> None:
        _pty_input_relay.restore_posix_terminal_input(self)

    def start_terminal_input_relay(
        self,
        *,
        arm_idle_timeout_on_submit: bool | None = None,
    ) -> None:
        _pty_input_relay.start_terminal_input_relay(
            self, arm_idle_timeout_on_submit=arm_idle_timeout_on_submit
        )

    def stop_terminal_input_relay(self) -> None:
        _pty_input_relay.stop_terminal_input_relay(self)

    def resize(self, rows: int, cols: int) -> None:
        self.rows = rows
        self.cols = cols
        if self._proc is None:
            return
        self._proc.resize(rows, cols)

    def send_interrupt(self) -> None:
        self._ensure_started()
        self.interrupt_count += 1
        self.interrupted_by_caller = True
        assert self._proc is not None
        self._proc.send_interrupt()

    def poll(self) -> int | None:
        if self._proc is None:
            return None
        return self._proc.poll()

    def wait(
        self, timeout: float | None = None, *, raise_on_abnormal_exit: bool = False
    ) -> int:
        self._ensure_started()
        assert self._proc is not None
        try:
            code = self._wait_for_exit_code(timeout=timeout)
        except TimeoutError:
            self.kill()
            self._finalize("timeout")
            raise TimeoutError("Pseudo-terminal process timed out") from None

        self._drain_native_until_eof(timeout=_PTY_READER_NATIVE_CLOSE_WAIT_SECONDS)
        self._finalize("exit")
        self._exit_status = classify_exit_status(code, KEYBOARD_INTERRUPT_EXIT_CODES)
        if code in KEYBOARD_INTERRUPT_EXIT_CODES:
            raise KeyboardInterrupt
        if raise_on_abnormal_exit and self._exit_status.abnormal:
            raise ProcessAbnormalExit(self._exit_status)
        return code

    def terminate(self) -> None:
        self._ensure_started()
        if self.poll() is not None:
            self._finalize("exit")
            return
        assert self._proc is not None
        self._proc.terminate()
        with suppress(TimeoutError, RuntimeError):
            self._wait_for_exit_code(timeout=2.0)
        self._drain_native_until_eof(timeout=_PTY_READER_NATIVE_CLOSE_WAIT_SECONDS)
        self._finalize("terminate")

    def kill(self) -> None:
        self._ensure_started()
        if self.poll() is not None:
            self._finalize("exit")
            return
        if sys.platform != "win32" and self.pid is not None:
            try:
                os.killpg(self.pid, signal.SIGKILL)
            except (OSError, AttributeError):
                pass
            else:
                with suppress(TimeoutError, RuntimeError):
                    self._wait_for_exit_code(timeout=2.0)
                with suppress(*_PTY_CLEANUP_ERRORS):
                    self._drain_native_until_eof(
                        timeout=_PTY_READER_NATIVE_CLOSE_WAIT_SECONDS
                    )
                self._finalize("kill")
                return
        assert self._proc is not None
        self._proc.kill()
        with suppress(TimeoutError, RuntimeError):
            self._wait_for_exit_code(timeout=2.0)
        self._drain_native_until_eof(timeout=_PTY_READER_NATIVE_CLOSE_WAIT_SECONDS)
        self._finalize("kill")

    def close(self) -> None:
        if self._proc is None:
            return
        if self._finalized:
            return
        with suppress(*_PTY_CLEANUP_ERRORS):
            if self.poll() is None:
                self._proc.close()
                self._drain_native_until_eof(
                    timeout=_PTY_READER_NATIVE_CLOSE_WAIT_SECONDS
                )
                self._finalize("close")
                return
            self._drain_native_until_eof(timeout=0.1)
            self._finalize("exit")

    def __del__(self) -> None:
        with suppress(*_PTY_CLEANUP_ERRORS):
            self.close()

    @property
    def pid(self) -> int | None:
        if self._proc is None:
            return None
        return self._proc.pid

    @property
    def output(self) -> str | bytes:
        if not self.capture:
            self._pump_native_output(timeout=0.0, consume_all=True)
            return b""
        self._pump_native_output(timeout=0.0, consume_all=True)
        value = self._buffer.output()
        if isinstance(value, str):
            return sanitize_for_encoding(value, self.encoding)
        return value

    @property
    def output_text(self) -> str:
        """Captured output decoded to ``str`` and sanitized for the parent console.

        Always safe to ``print()`` even when the parent console is cp1252 and
        the child emitted UTF-8.
        """
        raw = self.output
        if isinstance(raw, bytes):
            raw = raw.decode(self.encoding, self.errors)
        return sanitize_for_encoding(raw, self.encoding)

    def checkpoint(self) -> WaitCheckpoint:
        if not self.capture:
            raise NotImplementedError("PTY checkpoint() requires capture=True")
        return WaitCheckpoint(len(ensure_text(self.output, self.encoding, self.errors)))

    def wait_for_expect(
        self,
        next_expect: Expect | None = None,
        *,
        timeout: float | None = None,
        raise_on_abnormal_exit: bool = False,
        echo_output: bool = False,
    ) -> WaitForResult:
        return _pty_expect.wait_for_expect(
            self,
            next_expect,
            timeout=timeout,
            raise_on_abnormal_exit=raise_on_abnormal_exit,
            echo_output=echo_output,
        )

    @property
    def is_running(self) -> bool:
        return self.poll() is None

    def expect(
        self,
        pattern: ExpectPattern,
        *,
        timeout: float | None = None,
        action: ExpectAction = None,
    ) -> ExpectMatch:
        return _pty_expect.expect(self, pattern, timeout=timeout, action=action)

    @property
    def exit_status(self) -> ExitStatus | None:
        code = self.poll()
        if code is None:
            return None
        if self._exit_status is None:
            self._exit_status = classify_exit_status(
                code, KEYBOARD_INTERRUPT_EXIT_CODES
            )
        return self._exit_status

    def interrupt_and_wait(
        self,
        *,
        grace_timeout: float = 1.0,
        second_interrupt: bool = True,
        terminate_timeout: float | None = None,
        kill_timeout: float | None = None,
    ) -> InterruptResult:
        self.send_interrupt()
        if self._wait_until_exit(grace_timeout):
            return self._interrupt_result("interrupt")
        if second_interrupt:
            self.send_interrupt()
            second_interrupt_timeout = max(grace_timeout, 1.0)
            if self._wait_until_exit(second_interrupt_timeout):
                return self._interrupt_result("interrupt")
            if terminate_timeout is None and kill_timeout is None:
                self.kill()
                return self._interrupt_result("kill")
        if terminate_timeout is not None:
            self.terminate()
            if self._wait_until_exit(terminate_timeout):
                return self._interrupt_result("terminate")
        if kill_timeout is not None:
            self.kill()
            if self._wait_until_exit(kill_timeout):
                return self._interrupt_result("kill")
        return self._interrupt_result("interrupt")

    def wait_for_idle(
        self,
        idle_detector: IdleDetector | None = None,
        *,
        timeout: float | None = None,
        raise_on_abnormal_exit: bool = False,
        echo_output: bool = False,
    ) -> IdleWaitResult:
        return _pty_idle_waiter.wait_for_idle(
            self,
            idle_detector,
            timeout=timeout,
            raise_on_abnormal_exit=raise_on_abnormal_exit,
            echo_output=echo_output,
        )

    def wait_for(
        self,
        *conditions: (
            WaitCondition
            | Callable[..., object]
            | list[WaitCondition | Callable[..., object]]
            | tuple[WaitCondition | Callable[..., object], ...]
        ),
        timeout: float | None = None,
        raise_on_abnormal_exit: bool = False,
        echo_output: bool = False,
    ) -> WaitForResult:
        return _pty_wait_for.wait_for(
            self,
            *conditions,
            timeout=timeout,
            raise_on_abnormal_exit=raise_on_abnormal_exit,
            echo_output=echo_output,
        )

    def _read_chunk(self, *, timeout: float | None = None) -> bytes | None:
        return _pty_reader.read_chunk(self, timeout=timeout)

    def _ensure_started(self) -> None:
        if self._proc is None:
            raise RuntimeError("Pseudo-terminal process is not running")

    def _mark_native_stream_closed(self) -> None:
        if self._native_stream_closed:
            return
        self._native_stream_closed = True
        if self._buffer is not None:
            self._buffer.close()

    def _handle_native_chunk(self, chunk: bytes) -> None:
        if self._proc is not None:
            with suppress(RuntimeError):
                self._proc.respond_to_queries(chunk)
        # Output accounting (visible bytes, control churn) is now tracked
        # by the Rust reader thread via atomic counters.  Python only needs
        # to update the activity timestamp and echo/buffer bookkeeping.
        self.last_activity_at = time.time()
        self._pending_echo_chunks.append(chunk)
        if self._buffer is not None:
            self._buffer.record_output(chunk)
        if self._native_idle_detector is not None:
            self._native_idle_detector.record_output(chunk)

    def _echo_to_console(self, stream: TextIOBase) -> None:
        for chunk in self.drain_echo():
            _safe_console_write_chunk(
                stream,
                chunk,
                encoding=self.encoding,
                errors=self.errors,
            )

    def _pump_native_output(
        self,
        *,
        timeout: float | None,
        consume_all: bool,
    ) -> tuple[bool, bool]:
        return _pty_reader.pump_native_output(
            self, timeout=timeout, consume_all=consume_all
        )

    def _drain_native_until_eof(self, *, timeout: float) -> None:
        _pty_reader.drain_native_until_eof(self, timeout=timeout)

    def _prime_process_metrics(self) -> None:
        metrics = self._native_process_metrics
        if metrics is None:
            return
        metrics.prime()

    def _sample_idle_snapshot(
        self, process_cfg: ProcessIdleDetection | None
    ) -> _IdleSample:
        return _pty_idle_waiter.sample_idle_snapshot(self, process_cfg)

    def _wait_for_idle_native(
        self,
        idle_detector: IdleDetection,
        *,
        timeout: float | None,
    ) -> IdleWaitResult:
        return _pty_idle_waiter.wait_for_idle_native(
            self, idle_detector, timeout=timeout
        )

    def _start_native_exit_watcher(self) -> None:
        _pty_idle_waiter._start_native_exit_watcher(self)

    def _decode(self, data: bytes) -> str | bytes:
        if not self.text:
            return data
        return data.decode(self.encoding, self.errors)

    def _finalize(self, reason: str) -> None:
        if self._finalized:
            return
        self.stop_terminal_input_relay()
        self._finalized = True
        self._end_time = self._end_time or time.time()
        self.exit_reason = (
            "interrupt" if reason == "exit" and self.interrupted_by_caller else reason
        )
        if self.restore_terminal and not self._restored:
            self._restored = True

    def _interrupt_result(self, fallback_reason: str) -> InterruptResult:
        code = self.poll()
        if code is not None:
            self._drain_native_until_eof(timeout=_PTY_READER_NATIVE_CLOSE_WAIT_SECONDS)
            self._finalize("exit")
            code = self.poll()
        reason = self.exit_reason or fallback_reason
        self.exit_reason = reason
        return InterruptResult(
            reason,
            self.interrupt_count,
            code,
        )

    def _wait_until_exit(self, timeout: float) -> bool:
        self._ensure_started()
        try:
            self._wait_for_exit_code(timeout=timeout)
        except TimeoutError:
            return False
        self._drain_native_until_eof(timeout=_PTY_READER_NATIVE_CLOSE_WAIT_SECONDS)
        self._finalize("exit")
        return True

    def _wait_for_exit_code(self, *, timeout: float | None) -> int:
        self._ensure_started()
        deadline = None if timeout is None else time.monotonic() + max(0.0, timeout)
        while True:
            code = self.poll()
            if code is not None:
                return code
            if deadline is not None:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError("Pseudo-terminal process timed out")
                wait_timeout = min(0.05, remaining)
            else:
                wait_timeout = 0.05
            self._pump_native_output(timeout=wait_timeout, consume_all=True)
