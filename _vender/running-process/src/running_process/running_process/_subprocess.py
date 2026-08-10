from __future__ import annotations

from collections.abc import Callable
from pathlib import Path

from running_process.compat import CompletedProcess, TimeoutExpired
from running_process.priority import CpuPriority
from running_process.running_process._core import RunningProcess
from running_process.running_process._types import ProcessInfo


def subprocess_run(
    command: str | list[str],
    cwd: Path | None,
    check: bool,
    timeout: int,
    on_timeout: Callable[[ProcessInfo], None] | None = None,
    nice: int | CpuPriority | None = None,
) -> CompletedProcess[str]:
    try:
        return RunningProcess.run(
            command,
            cwd=cwd,
            check=check,
            timeout=timeout,
            capture_output=True,
            on_timeout=on_timeout,
            nice=nice,
        )
    except TimeoutExpired as exc:
        raise RuntimeError(
            f"CRITICAL: Process timed out after {timeout} seconds: {command}"
        ) from exc
