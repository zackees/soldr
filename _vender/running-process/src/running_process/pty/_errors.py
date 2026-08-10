from __future__ import annotations

from running_process._native import NativeSignalBool


class PtyNotAvailableError(RuntimeError):
    pass


class SignalBool:
    def __init__(self, value: bool = False) -> None:
        self._value = bool(value)
        self._native = NativeSignalBool(self._value)

    @property
    def value(self) -> bool:
        return self._value

    @value.setter
    def value(self, value: bool) -> None:
        self._value = bool(value)
        self._native.value = self._value

    def load(self) -> bool:
        return self._native.load_nolock()

    def store(self, value: bool) -> None:
        self.value = value

    def compare_and_swap(self, current: bool, new: bool) -> bool:
        swapped = self._native.compare_and_swap_locked(bool(current), bool(new))
        if swapped:
            self._value = bool(new)
        else:
            self._value = self._native.load_nolock()
        return swapped

    def __bool__(self) -> bool:
        return self._value
