from __future__ import annotations

import subprocess as _subprocess
from typing import Any, Generic, TypeVar

T = TypeVar("T")

PIPE = _subprocess.PIPE
STDOUT = _subprocess.STDOUT
DEVNULL = _subprocess.DEVNULL
CREATE_NEW_PROCESS_GROUP = getattr(_subprocess, "CREATE_NEW_PROCESS_GROUP", 0)


class CompletedProcess(_subprocess.CompletedProcess[T], Generic[T]):
    pass


class CalledProcessError(_subprocess.CalledProcessError):
    pass


class TimeoutExpired(_subprocess.TimeoutExpired):
    pass


def make_completed_process(
    args: str | list[str],
    returncode: int,
    stdout: Any = None,
    stderr: Any = None,
) -> CompletedProcess[Any]:
    return CompletedProcess(
        args=args,
        returncode=returncode,
        stdout=stdout,
        stderr=stderr,
    )
