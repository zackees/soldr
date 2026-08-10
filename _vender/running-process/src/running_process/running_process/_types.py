from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any, NamedTuple

if TYPE_CHECKING:
    from running_process.running_process._core import RunningProcess


_FINALIZER_CLEANUP_ERRORS = (OSError, RuntimeError, TimeoutError, ValueError, AttributeError)


class EndOfStream:
    def __repr__(self) -> str:
        return "EOS"


EOS = EndOfStream()


@dataclass
class ProcessInfo:
    pid: int
    command: str | list[str]
    duration: float


EchoValue = str | bytes
EchoCallback = Callable[[str], None]


def _is_eos(value: object) -> bool:
    return isinstance(value, EndOfStream)


class ProcessOutputEvent(NamedTuple):
    stdout: EchoValue | EndOfStream | None
    stderr: EchoValue | EndOfStream | None
    exit_code: int | None

    @property
    def streams_drained(self) -> bool:
        return (self.stdout is None or _is_eos(self.stdout)) and (
            self.stderr is None or _is_eos(self.stderr)
        )

    @property
    def finished_and_drained(self) -> bool:
        return self.streams_drained and self.exit_code is not None


class CapturedProcessStream:
    def __init__(self, process: RunningProcess, stream: str) -> None:
        self._process = process
        self._stream = stream

    def available(self) -> bool:
        if self._stream == "combined":
            return self._process.has_pending_output()
        if self._stream == "stdout":
            return self._process.has_pending_stdout()
        return self._process.has_pending_stderr()

    def read(self) -> str | bytes:
        return self._process._captured_stream_value(self._stream)

    def drain(self) -> list[EchoValue] | list[tuple[str, EchoValue]]:
        if self._stream == "stdout":
            return self._process.drain_stdout()
        if self._stream == "stderr":
            return self._process.drain_stderr()
        return self._process.drain_combined()

    def __repr__(self) -> str:
        return repr(self.read())

    def __str__(self) -> str:
        value = self.read()
        if isinstance(value, str):
            return value
        return value.decode(self._process.encoding, self._process.errors)

    def __bytes__(self) -> bytes:
        value = self.read()
        if isinstance(value, bytes):
            return value
        return value.encode(self._process.encoding, self._process.errors)

    def __eq__(self, other: object) -> bool:
        return self.read() == other

    def __bool__(self) -> bool:
        return bool(self.read())

    def __len__(self) -> int:
        return len(self.read())

    def __contains__(self, item: object) -> bool:
        value = self.read()
        return item in value  # type: ignore[operator]

    def __getattr__(self, name: str) -> Any:
        return getattr(self.read(), name)
