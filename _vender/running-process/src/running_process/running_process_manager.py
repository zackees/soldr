from __future__ import annotations

import os
import time
import warnings
from dataclasses import dataclass

from running_process._native import (
    native_list_active_processes,
    native_process_created_at,
    native_register_process,
    native_unregister_process,
)
from running_process.process_utils import kill_process_tree


@dataclass(frozen=True, slots=True)
class ActiveProcessInfo:
    pid: int
    kind: str
    command: str
    cwd: str | None
    start_time: float

    @property
    def finished(self) -> bool:
        return False

    @property
    def duration(self) -> float:
        return max(0.0, time.time() - self.start_time)

    def kill(self) -> None:
        kill_process_tree(self.pid)


class RunningProcessManager:
    def register(self, proc: object) -> None:
        pid = getattr(proc, "pid", None)
        if pid is None:
            return
        command_value = getattr(proc, "command", None)
        if isinstance(command_value, list):
            command = " ".join(str(part) for part in command_value)
        else:
            command = str(command_value)
        cwd = getattr(proc, "cwd", None)
        if isinstance(cwd, os.PathLike):
            cwd = os.fspath(cwd)
        use_pty = bool(getattr(proc, "use_pty", False))
        kind = "pty" if use_pty else "subprocess"
        native_register_process(int(pid), kind, command, cwd)

    def unregister(self, proc: object) -> None:
        pid = getattr(proc, "pid", None)
        if pid is None:
            return
        native_unregister_process(int(pid))

    def list_active(self) -> list[ActiveProcessInfo]:
        active: list[ActiveProcessInfo] = []
        stale_pids: list[int] = []
        for row in native_list_active_processes():
            info = ActiveProcessInfo(*row)
            if native_process_created_at(info.pid) is not None:
                active.append(info)
            else:
                stale_pids.append(info.pid)
        for pid in stale_pids:
            native_unregister_process(pid)
        return active

    def dump_active(self) -> None:
        active = self.list_active()
        if not active:
            warnings.warn("NO ACTIVE SUBPROCESSES DETECTED", UserWarning, stacklevel=2)
            return

        warnings.warn("STUCK SUBPROCESS COMMANDS:", UserWarning, stacklevel=2)
        for index, proc in enumerate(active, start=1):
            warnings.warn(
                f"  {index}. cmd={proc.command} pid={proc.pid} duration={proc.duration:.1f}s",
                UserWarning,
                stacklevel=2,
            )


RunningProcessManagerSingleton = RunningProcessManager()
