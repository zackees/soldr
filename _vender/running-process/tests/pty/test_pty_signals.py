"""Signals: SignalBool, console isolated process group, killpg, send_interrupt."""

from __future__ import annotations

import sys
from types import SimpleNamespace

import pytest

# NOTE: After the #151 monolith split, InteractiveProcess and
# PseudoTerminalProcess live in separate sub-modules. Tests below patch
# the sub-module that owns the symbol under test.
import running_process.pty._interactive as interactive_module
import running_process.pty._pseudo_terminal as pty_module
from running_process import (
    InteractiveMode,
    RunningProcess,
)
from running_process.pty import (
    InteractiveProcess,
    PseudoTerminalProcess,
)


def test_running_process_signal_bool_shadows_python_reads() -> None:
    signal = RunningProcess.SignalBool(False)
    assert signal.value is False
    assert bool(signal) is False
    assert signal.load() is False

    signal.store(True)

    assert signal.value is True
    assert bool(signal) is True
    assert signal.load() is True
    assert signal.compare_and_swap(True, False) is True
    assert signal.value is False
    assert signal.compare_and_swap(True, True) is False
    assert signal.value is False


def test_console_isolated_uses_process_group_on_posix(monkeypatch: pytest.MonkeyPatch) -> None:
    captured: dict[str, object] = {}

    class FakeProc:
        pid = 1234

        def __init__(self, command: object, **kwargs: object) -> None:
            captured["command"] = command
            captured.update(kwargs)

        def poll(self) -> int | None:
            return 0

        def start(self) -> None:
            return None

    monkeypatch.setattr(interactive_module.sys, "platform", "linux")
    monkeypatch.setattr(interactive_module, "NativeProcess", FakeProc)

    process = InteractiveProcess(
        [sys.executable, "-c", "print('x')"],
        mode=InteractiveMode.CONSOLE_ISOLATED,
    )

    assert captured["create_process_group"] is True
    assert process.pid == 1234


def test_console_isolated_send_interrupt_uses_killpg_on_posix(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    calls: list[tuple[int, int]] = []
    monkeypatch.setattr(interactive_module.sys, "platform", "linux")
    monkeypatch.setattr(
        interactive_module.os, "killpg", lambda pid, sig: calls.append((pid, sig)), raising=False
    )
    monkeypatch.setattr(interactive_module.signal, "SIGINT", 2, raising=False)

    process = InteractiveProcess(
        [sys.executable, "-c", "print('x')"],
        mode=InteractiveMode.CONSOLE_ISOLATED,
        auto_run=False,
    )
    process._proc = SimpleNamespace(pid=4321)
    process.send_interrupt()

    assert calls == [(4321, interactive_module.signal.SIGINT)]


def test_pseudo_terminal_kill_uses_killpg_on_posix(monkeypatch: pytest.MonkeyPatch) -> None:
    calls: list[tuple[int, int]] = []
    state = {"alive": True}
    monkeypatch.setattr(pty_module.sys, "platform", "linux")
    monkeypatch.setattr(pty_module.Pty, "is_available", classmethod(lambda cls: True))
    monkeypatch.setattr(
        pty_module.os,
        "killpg",
        lambda pid, sig: (calls.append((pid, sig)), state.__setitem__("alive", False)),
        raising=False,
    )
    monkeypatch.setattr(pty_module.signal, "SIGKILL", 9, raising=False)

    class FakeProc:
        pid = 2468

        def poll(self) -> int | None:
            return None if state["alive"] else -9

    process = PseudoTerminalProcess(
        [sys.executable, "-c", "print('x')"],
        auto_run=False,
    )
    process._proc = FakeProc()
    process.kill()

    assert calls == [(2468, pty_module.signal.SIGKILL)]


def test_pseudo_terminal_send_interrupt_delegates_to_native_process() -> None:
    calls: list[str] = []
    process = PseudoTerminalProcess(
        [sys.executable, "-c", "print('x')"],
        auto_run=False,
    )
    process._proc = SimpleNamespace(send_interrupt=lambda: calls.append("interrupt"))

    process.send_interrupt()

    assert calls == ["interrupt"]
    assert process.interrupt_count == 1
    assert process.interrupted_by_caller is True
