from __future__ import annotations

import time
from typing import TYPE_CHECKING

from running_process.running_process._types import EOS, ProcessOutputEvent
from running_process.running_process_manager import RunningProcessManagerSingleton

if TYPE_CHECKING:
    from running_process.running_process._core import RunningProcess


class _RunningProcessOutputIterator:
    def __init__(self, process: RunningProcess, timeout: float | None) -> None:
        self._process = process
        self._timeout = timeout
        self._streams_drained = False
        self._finished = False

    def __iter__(self) -> _RunningProcessOutputIterator:
        return self

    def __next__(self) -> ProcessOutputEvent:
        if self._finished:
            raise StopIteration
        if self._process._pty_process is not None:
            raise NotImplementedError(
                "stdout/stderr tuple iteration is only available for pipe-backed RunningProcess"
            )
        if not self._process.capture:
            raise NotImplementedError("stdout/stderr tuple iteration requires capture=True")
        if self._streams_drained:
            exit_code = self._process.poll()
            if exit_code is None:
                exit_code = self._process._proc.wait(timeout=self._timeout)
                self._process._end_time = self._process._end_time or time.time()
                RunningProcessManagerSingleton.unregister(self._process)
            self._finished = True
            return ProcessOutputEvent(EOS, EOS, exit_code)

        status, stream, line = self._process._proc.take_combined_line(self._timeout)
        if status == "timeout":
            raise TimeoutError("No stdout or stderr available before timeout")
        if status == "line" and stream is not None and line is not None:
            exit_code = self._process.returncode
            if stream == "stdout":
                return ProcessOutputEvent(self._process._format(line), None, exit_code)
            return ProcessOutputEvent(None, self._process._format(line), exit_code)
        exit_code = self._process.poll()

        self._streams_drained = True
        if exit_code is None:
            try:
                grace_timeout = 0.01
                if self._timeout is not None:
                    grace_timeout = min(self._timeout, grace_timeout)
                exit_code = self._process._proc.wait(timeout=grace_timeout)
                self._process._end_time = self._process._end_time or time.time()
                RunningProcessManagerSingleton.unregister(self._process)
            except TimeoutError:
                exit_code = None
        if exit_code is not None:
            self._finished = True
        return ProcessOutputEvent(EOS, EOS, exit_code)
