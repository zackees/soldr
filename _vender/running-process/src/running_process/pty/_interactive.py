from __future__ import annotations

import os
import signal
import sys
import time
from collections.abc import Mapping
from contextlib import suppress
from pathlib import Path

from running_process._native import NativeProcess
from running_process.exit_status import ExitStatus, ProcessAbnormalExit, classify_exit_status
from running_process.priority import CpuPriority, normalize_nice
from running_process.pty._command import _normalize_command, interactive_launch_spec
from running_process.pty._process_helpers import _PTY_CLEANUP_ERRORS
from running_process.pty._pseudo_terminal import KEYBOARD_INTERRUPT_EXIT_CODES
from running_process.pty._types import InteractiveMode


class InteractiveProcess:
    def __init__(
        self,
        command: str | list[str],
        *,
        mode: InteractiveMode | str = InteractiveMode.CONSOLE_SHARED,
        cwd: str | Path | None = None,
        shell: bool | None = None,
        env: Mapping[str, str] | None = None,
        nice: int | CpuPriority | None = None,
        restore_terminal: bool | None = None,
        auto_run: bool = True,
    ) -> None:
        resolved_mode = InteractiveMode(mode)
        if resolved_mode is InteractiveMode.PSEUDO_TERMINAL:
            raise ValueError("Use PseudoTerminalProcess for pseudo_terminal mode")

        command, shell = _normalize_command(command, shell)
        self.command = command
        self.shell = shell
        self.cwd = str(cwd) if cwd is not None else None
        self.env = dict(env) if env is not None else os.environ.copy()
        self.nice = normalize_nice(nice)
        self.launch_spec = interactive_launch_spec(resolved_mode)
        self.restore_terminal = (
            self.launch_spec.restore_terminal
            if restore_terminal is None
            else restore_terminal
        )
        self._proc: NativeProcess | None = None
        self._end_time: float | None = None
        self._finalized = False
        self.exit_reason: str | None = None
        self.interrupt_count = 0
        self.interrupted_by_caller = False
        self._exit_status: ExitStatus | None = None

        if auto_run:
            self.start()

    def start(self) -> None:
        if self._proc is not None:
            raise RuntimeError("Interactive process already started")
        creationflags = self.launch_spec.creationflags if sys.platform == "win32" else None
        self._proc = NativeProcess(
            self.command,
            cwd=self.cwd,
            env=self.env,
            shell=self.shell,
            capture=False,
            creationflags=creationflags,
            nice=self.nice,
            create_process_group=(
                sys.platform != "win32"
                and self.launch_spec.mode is InteractiveMode.CONSOLE_ISOLATED
            ),
        )
        self._proc.start()

    def poll(self) -> int | None:
        if self._proc is None:
            return None
        return self._proc.poll()

    def wait(self, timeout: float | None = None, *, raise_on_abnormal_exit: bool = False) -> int:
        if self._proc is None:
            raise RuntimeError("Interactive process is not running")
        try:
            code = self._proc.wait(timeout=timeout)
        except TimeoutError as exc:
            self.kill()
            self._finalize("timeout")
            raise TimeoutError("Interactive process timed out") from exc
        self._finalize("exit")
        self._exit_status = classify_exit_status(code, KEYBOARD_INTERRUPT_EXIT_CODES)
        if code in KEYBOARD_INTERRUPT_EXIT_CODES:
            raise KeyboardInterrupt
        if raise_on_abnormal_exit and self._exit_status.abnormal:
            raise ProcessAbnormalExit(self._exit_status)
        return code

    def terminate(self) -> None:
        if self._proc is None:
            raise RuntimeError("Interactive process is not running")
        if self.poll() is not None:
            self._finalize("exit")
            return
        if self.launch_spec.mode is InteractiveMode.CONSOLE_ISOLATED:
            self._proc.terminate_group()
        else:
            self._proc.terminate()
        self._wait_for_exit()
        self._finalize("terminate")

    def kill(self) -> None:
        if self._proc is None:
            raise RuntimeError("Interactive process is not running")
        if self.poll() is not None:
            self._finalize("exit")
            return
        if self.launch_spec.mode is InteractiveMode.CONSOLE_ISOLATED:
            self._proc.kill_group()
        else:
            self._proc.kill()
        self._wait_for_exit()
        self._finalize("kill")

    def close(self) -> None:
        if self._proc is None or self._finalized:
            return
        with suppress(*_PTY_CLEANUP_ERRORS):
            if self.poll() is None:
                self.kill()
                return
            self._finalize("exit")

    def __del__(self) -> None:
        with suppress(*_PTY_CLEANUP_ERRORS):
            self.close()

    def send_interrupt(self) -> None:
        if self._proc is None:
            raise RuntimeError("Interactive process is not running")
        self.interrupt_count += 1
        self.interrupted_by_caller = True
        if (
            sys.platform != "win32"
            and self.launch_spec.mode is InteractiveMode.CONSOLE_ISOLATED
            and self.pid is not None
        ):
            with suppress(OSError, AttributeError):
                os.killpg(self.pid, signal.SIGINT)
                return
        self._proc.send_interrupt()

    @property
    def pid(self) -> int | None:
        if self._proc is None:
            return None
        return self._proc.pid

    @property
    def exit_status(self) -> ExitStatus | None:
        code = self.poll()
        if code is None:
            return None
        if self._exit_status is None:
            self._exit_status = classify_exit_status(code, KEYBOARD_INTERRUPT_EXIT_CODES)
        return self._exit_status

    def _finalize(self, reason: str) -> None:
        if self._finalized:
            return
        self._finalized = True
        self._end_time = time.time()
        self.exit_reason = (
            "interrupt" if reason == "exit" and self.interrupted_by_caller else reason
        )

    def _wait_for_exit(self) -> None:
        try:
            self._proc.wait(timeout=2)
        except TimeoutError:
            self._proc.kill()
            self._proc.wait(timeout=2)
