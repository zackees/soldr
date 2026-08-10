from __future__ import annotations

from dataclasses import dataclass


@dataclass(frozen=True)
class _BufferedInput:
    data: str | bytes
    submit: bool = False


class WaitInputBuffer:
    def __init__(self) -> None:
        self._items: list[str | bytes | _BufferedInput] = []

    def write(self, data: str | bytes) -> None:
        self._items.append(data)

    def submit(self, data: str | bytes) -> None:
        self._items.append(_BufferedInput(data=data, submit=True))

    def drain(self) -> list[str | bytes | _BufferedInput]:
        items = list(self._items)
        self._items.clear()
        return items

    def __bool__(self) -> bool:
        return bool(self._items)
