from __future__ import annotations

from running_process import NativeTerminalInput, NativeTerminalInputEvent


def test_native_terminal_input_is_exported() -> None:
    capture = NativeTerminalInput()
    assert isinstance(capture, NativeTerminalInput)
    assert capture.capturing is False


def test_native_terminal_input_event_type_is_exported() -> None:
    assert NativeTerminalInputEvent.__name__ == "NativeTerminalInputEvent"
