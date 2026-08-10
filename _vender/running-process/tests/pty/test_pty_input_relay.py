"""Windows native terminal input relay, terminal input events, CRLF handling."""

from __future__ import annotations

import sys
import time

import pytest
from running_process._native import native_windows_terminal_input_bytes

# NOTE: After the #151 monolith split, the symbols these tests patch live
# in sub-modules rather than the package namespace. Aim each monkeypatch
# at the submodule that actually imports the symbol so the patch reaches
# the production lookup site.
import running_process.pty._pseudo_terminal as pty_module
import running_process.running_process._core as running_process_module
from running_process import RunningProcess
from running_process.pty import PseudoTerminalProcess


def test_running_process_use_pty_forwards_terminal_input_relay_options(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    recorded: dict[str, object] = {}

    class FakePtyProcess:
        def __init__(self, command: str | list[str], **kwargs: object) -> None:
            recorded["command"] = command
            recorded["kwargs"] = kwargs

        def start(self) -> None:
            return None

    monkeypatch.setattr(running_process_module, "PseudoTerminalProcess", FakePtyProcess)

    process = RunningProcess(
        [sys.executable, "-c", "print('relay')"],
        use_pty=True,
        auto_run=False,
        relay_terminal_input=True,
        arm_idle_timeout_on_submit=True,
    )

    assert process._pty_process is not None
    kwargs = recorded["kwargs"]
    assert isinstance(kwargs, dict)
    assert kwargs["relay_terminal_input"] is True
    assert kwargs["arm_idle_timeout_on_submit"] is True


def test_running_process_pseudo_terminal_forwards_terminal_input_relay_options(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    recorded: dict[str, object] = {}

    class FakePtyProcess:
        def __init__(self, command: str | list[str], **kwargs: object) -> None:
            recorded["command"] = command
            recorded["kwargs"] = kwargs

    monkeypatch.setattr(running_process_module, "PseudoTerminalProcess", FakePtyProcess)

    process = RunningProcess.pseudo_terminal(
        [sys.executable, "-c", "print('relay')"],
        auto_run=False,
        relay_terminal_input=True,
        arm_idle_timeout_on_submit=True,
    )

    kwargs = recorded["kwargs"]
    assert isinstance(kwargs, dict)
    assert kwargs["relay_terminal_input"] is True
    assert kwargs["arm_idle_timeout_on_submit"] is True
    assert isinstance(process, FakePtyProcess)


def test_pseudo_terminal_windows_native_input_relay_forwards_events_and_arms_idle_timeout(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    captures: list[FakeCapture] = []

    class FakeCapture:
        def __init__(self) -> None:
            self.started = False
            self.closed = False
            self._reads = 0
            captures.append(self)

        def start(self) -> None:
            self.started = True

        def read_batch(self, timeout: float | None = None) -> tuple[bytes, bool]:
            del timeout
            self._reads += 1
            if self._reads == 1:
                return (b"hello\r", True)
            raise TimeoutError

        def close(self) -> None:
            self.closed = True

    class FakeProc:
        def __init__(self, owner: PseudoTerminalProcess) -> None:
            self.owner = owner
            self.pid = 1234

        def poll(self) -> int | None:
            if self.owner._terminal_input_stop.is_set():
                return 0
            return None

    monkeypatch.setattr(pty_module, "NativeTerminalInput", FakeCapture)
    monkeypatch.setattr(pty_module.sys, "platform", "win32")

    process = PseudoTerminalProcess(
        [sys.executable, "-c", "print('relay')"],
        auto_run=False,
    )
    process._proc = FakeProc(process)  # type: ignore[assignment]
    process.idle_timeout_enabled = False
    writes: list[tuple[bytes, bool]] = []

    def fake_write(data: str | bytes, *, submit: bool = False) -> None:
        raw = data.encode(process.encoding, process.errors) if isinstance(data, str) else data
        writes.append((raw, submit))
        process._terminal_input_stop.set()

    process.write = fake_write  # type: ignore[method-assign]
    process.start_terminal_input_relay(arm_idle_timeout_on_submit=True)

    deadline = time.time() + 1.0
    while process.terminal_input_relay_active and time.time() < deadline:
        time.sleep(0.01)

    capture = captures[0]
    process.stop_terminal_input_relay()

    assert capture.started is True
    assert capture.closed is True
    assert writes == [(b"hello\r", True)]
    assert process.idle_timeout_enabled is True


def test_pseudo_terminal_windows_native_process_relay_api_owns_relay_lifecycle(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    class FakeProc:
        def __init__(self) -> None:
            self.started = False
            self.stopped = False
            self.active_checks = 0
            self.input_bytes_total = 0
            self.newline_events_total = 0
            self.submit_events_total = 0

        def start_terminal_input_relay(self) -> None:
            self.started = True

        def stop_terminal_input_relay(self) -> None:
            self.stopped = True

        def terminal_input_relay_active(self) -> bool:
            if not self.started:
                return False
            self.active_checks += 1
            if self.active_checks == 1:
                self.input_bytes_total = 6
                self.newline_events_total = 1
                self.submit_events_total = 1
                return True
            return False

        def pty_input_bytes_total(self) -> int:
            return self.input_bytes_total

        def pty_newline_events_total(self) -> int:
            return self.newline_events_total

        def pty_submit_events_total(self) -> int:
            return self.submit_events_total

    monkeypatch.setattr(pty_module.sys, "platform", "win32")
    monkeypatch.setattr(
        pty_module,
        "NativeTerminalInput",
        lambda: (_ for _ in ()).throw(AssertionError("python relay should stay unused")),
    )

    process = PseudoTerminalProcess(
        [sys.executable, "-c", "print('relay')"],
        auto_run=False,
    )
    process._proc = FakeProc()  # type: ignore[assignment]
    process.idle_timeout_enabled = False

    process.start_terminal_input_relay(arm_idle_timeout_on_submit=True)

    assert process.terminal_input_relay_active is True
    assert process.idle_timeout_enabled is True
    assert process._pty_input_bytes_total == 6
    assert process._pty_newline_events_total == 1
    assert process._pty_submit_events_total == 1
    assert process.terminal_input_relay_active is False

    process.stop_terminal_input_relay()

    assert process._proc.started is True  # type: ignore[union-attr]
    assert process._proc.stopped is True  # type: ignore[union-attr]


def test_pseudo_terminal_windows_native_input_relay_preserves_shift_enter_vs_enter(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    captures: list[FakeCapture] = []

    class FakeCapture:
        def __init__(self) -> None:
            self.started = False
            self.closed = False
            self._batches = iter(
                [
                    # read_batch merges all queued events in Rust; the relay
                    # receives a single merged batch per call.
                    (b"hello\x1b[13;2uworld\r", True),
                ]
            )
            captures.append(self)

        def start(self) -> None:
            self.started = True

        def read_batch(self, timeout: float | None = None) -> tuple[bytes, bool]:
            del timeout
            try:
                return next(self._batches)
            except StopIteration as exc:
                raise TimeoutError from exc

        def close(self) -> None:
            self.closed = True

    class FakeProc:
        def __init__(self, owner: PseudoTerminalProcess) -> None:
            self.owner = owner
            self.pid = 1234

        def poll(self) -> int | None:
            if self.owner._terminal_input_stop.is_set():
                return 0
            return None

    monkeypatch.setattr(pty_module, "NativeTerminalInput", FakeCapture)
    monkeypatch.setattr(pty_module.sys, "platform", "win32")

    process = PseudoTerminalProcess(
        [sys.executable, "-c", "print('relay')"],
        auto_run=False,
    )
    process._proc = FakeProc(process)  # type: ignore[assignment]
    process.idle_timeout_enabled = False
    writes: list[tuple[bytes, bool]] = []

    def fake_write(data: str | bytes, *, submit: bool = False) -> None:
        raw = data.encode(process.encoding, process.errors) if isinstance(data, str) else data
        writes.append((raw, submit))
        process._terminal_input_stop.set()

    process.write = fake_write  # type: ignore[method-assign]
    process.start_terminal_input_relay(arm_idle_timeout_on_submit=True)

    deadline = time.time() + 1.0
    while process.terminal_input_relay_active and time.time() < deadline:
        time.sleep(0.01)

    capture = captures[0]
    process.stop_terminal_input_relay()

    assert capture.started is True
    assert capture.closed is True
    # All events batched into a single write; shift-enter sequence preserved.
    assert writes == [(b"hello\x1b[13;2uworld\r", True)]
    assert process.idle_timeout_enabled is True


def test_windows_terminal_input_bytes_preserves_explicit_crlf() -> None:
    assert native_windows_terminal_input_bytes(b"a\r\nb") == b"a\r\nb"
    expected = b"a\rb" if sys.platform == "win32" else b"a\nb"
    assert native_windows_terminal_input_bytes(b"a\nb") == expected
