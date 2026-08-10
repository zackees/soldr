from __future__ import annotations

from collections.abc import Iterator
from contextlib import AbstractContextManager
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from running_process.running_process import RunningProcess


class _RunningProcessLineIterator(AbstractContextManager[Iterator[str]], Iterator[str]):
    def __init__(self, rp: RunningProcess, timeout: float | None) -> None:
        self._rp = rp
        self._timeout = timeout

    def __enter__(self) -> _RunningProcessLineIterator:
        return self

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        tb: Any | None,
    ) -> bool:
        return False

    def __iter__(self) -> Iterator[str]:
        return self

    def __next__(self) -> str:
        item = self._rp.get_next_line(timeout=self._timeout)
        if isinstance(item, self._rp.end_of_stream_type):
            raise StopIteration
        return item
