from __future__ import annotations

import time
from typing import Protocol


class OutputFormatter(Protocol):
    def begin(self) -> None: ...

    def transform(self, line: str) -> str: ...

    def end(self) -> None: ...


class NullOutputFormatter:
    def begin(self) -> None:
        return None

    def transform(self, line: str) -> str:
        return line

    def end(self) -> None:
        return None


class TimeDeltaFormatter:
    def __init__(self, start_time: float | None = None) -> None:
        self._start_time = start_time

    def begin(self) -> None:
        if self._start_time is None:
            self._start_time = time.time()

    def transform(self, line: str) -> str:
        if self._start_time is None:
            self._start_time = time.time()
        return f"[{time.time() - self._start_time:.2f}] {line}"

    def end(self) -> None:
        return None
