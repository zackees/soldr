"""wait_for_expect / wait_for: chaining, timeout, registered expects, callbacks."""

from __future__ import annotations

import contextlib
import sys
import time
from types import SimpleNamespace

import pytest

# `time.time` calls inside PseudoTerminalProcess.wait_for_expect live in
# the `_pseudo_terminal` sub-module after the #151 refactor; patch there.
import running_process.pty._pseudo_terminal as pty_module
from running_process import (
    Expect,
    ExpectRule,
    Idle,
    IdleDetection,
    IdleTiming,
    RunningProcess,
    WaitCallbackResult,
)
from running_process.pty import (
    IdleWaitResult,
    PseudoTerminalProcess,
    WaitForResult,
)


def test_pseudo_terminal_wait_for_expect_on_callback_can_continue_until_exit(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    seen: list[str] = []
    writes: list[tuple[str | bytes, bool]] = []
    history_updates = iter(
        [
            ("tick>", 5),
            ("tick>", 10),
        ]
    )
    process = PseudoTerminalProcess(
        [sys.executable, "-c", "print('x')"],
        text=True,
        auto_run=False,
    )

    class FakeProc:
        pid = 1234

        def poll(self) -> int | None:
            return 0

        def close(self) -> None:
            return None

    process._proc = FakeProc()  # type: ignore[assignment]
    monkeypatch.setattr(process, "_pump_native_output", lambda timeout, consume_all: None)
    monkeypatch.setattr(process, "_drain_native_until_eof", lambda timeout: None)
    monkeypatch.setattr(process, "_snapshot_output_history", lambda: ("", 0))

    def snapshot_output_since(_start: int) -> tuple[str, int]:
        try:
            return next(history_updates)
        except StopIteration:
            return ("", 10)

    def fake_write(data: str | bytes, *, submit: bool = False) -> None:
        writes.append((data, submit))

    monkeypatch.setattr(process, "_snapshot_output_since", snapshot_output_since)
    process.write = fake_write  # type: ignore[method-assign]

    def hook(match) -> WaitCallbackResult:
        seen.append(match.matched)
        if len(seen) == 1:
            return WaitCallbackResult.CONTINUE
        return WaitCallbackResult.EXIT

    result = process.wait_for(
        Expect("tick>", action="go\n", on_callback=hook),
        timeout=5.0,
    )

    assert isinstance(result, WaitForResult)
    assert result.matched is True
    assert result.condition is not None
    assert isinstance(result.condition, Expect)
    assert result.expect_match is not None
    assert result.expect_match.matched == "tick>"
    assert seen == ["tick>", "tick>"]
    assert writes == [("go\n", False), ("go\n", False)]


def test_pseudo_terminal_wait_for_expect_not_suppresses_trigger() -> None:
    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            (
                "import sys\n"
                "sys.stdout.write('ERROR>'); sys.stdout.flush()\n"
                "sys.stdout.write('DONE>'); sys.stdout.flush()\n"
            ),
        ],
        text=True,
    )
    result = process.wait_for(
        Expect("DONE>", NOT="ERROR>"),
        timeout=5.0,
    )

    assert result.matched is False
    assert result.exit_reason == "process_exit"
    assert result.returncode == 0


def test_pseudo_terminal_wait_for_idle_on_callback_can_disarm_and_allow_expect(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    writes: list[tuple[str | bytes, bool]] = []
    idle_disarmed = False
    delivered_output = False
    snapshot_values = [
        SimpleNamespace(
            sampled_at=0.00,
            process_alive=True,
            pty_input_bytes=0,
            pty_output_bytes=0,
            pty_control_churn_bytes=0,
            cpu_percent=0.0,
            disk_io_bytes=0,
            network_io_bytes=0,
            returncode=None,
        ),
        SimpleNamespace(
            sampled_at=0.08,
            process_alive=True,
            pty_input_bytes=0,
            pty_output_bytes=0,
            pty_control_churn_bytes=0,
            cpu_percent=0.0,
            disk_io_bytes=0,
            network_io_bytes=0,
            returncode=None,
        ),
        SimpleNamespace(
            sampled_at=0.11,
            process_alive=True,
            pty_input_bytes=0,
            pty_output_bytes=0,
            pty_control_churn_bytes=0,
            cpu_percent=0.0,
            disk_io_bytes=0,
            network_io_bytes=0,
            returncode=None,
        ),
    ]
    snapshots = iter(snapshot_values)
    last_snapshot = snapshot_values[-1]

    process = PseudoTerminalProcess(
        [sys.executable, "-c", "print('x')"],
        text=True,
        auto_run=False,
    )

    class FakeProc:
        pid = 1234

        def poll(self) -> int | None:
            return None

        def close(self) -> None:
            return None

    process._proc = FakeProc()  # type: ignore[assignment]
    monkeypatch.setattr(process, "_pump_native_output", lambda timeout, consume_all: None)

    fake_now = -0.01

    def fake_time() -> float:
        nonlocal fake_now
        fake_now += 0.01
        return fake_now

    monkeypatch.setattr(pty_module.time, "time", fake_time)

    def sample_idle_snapshot(process_cfg=None):
        try:
            return next(snapshots)
        except StopIteration:
            return last_snapshot

    monkeypatch.setattr(process, "_sample_idle_snapshot", sample_idle_snapshot)
    monkeypatch.setattr(process, "_snapshot_output_history", lambda: ("", 0))

    def snapshot_output_since(start: int) -> tuple[str, int]:
        nonlocal delivered_output
        if idle_disarmed and not delivered_output:
            delivered_output = True
            return ("DONE>", 5)
        return ("", start)

    def fake_write(data: str | bytes, *, submit: bool = False) -> None:
        writes.append((data, submit))

    monkeypatch.setattr(process, "_snapshot_output_since", snapshot_output_since)
    process.write = fake_write  # type: ignore[method-assign]

    def disarm_idle(_result: IdleWaitResult) -> WaitCallbackResult:
        nonlocal idle_disarmed
        idle_disarmed = True
        return WaitCallbackResult.CONTINUE_AND_DISARM

    result = process.wait_for(
        Idle(
            IdleDetection(
                timing=IdleTiming(
                    timeout_seconds=0.05,
                    stability_window_seconds=0.02,
                    sample_interval_seconds=0.01,
                )
            ),
            on_callback=disarm_idle,
        ),
        Expect("DONE>", action="\n"),
        timeout=5.0,
    )

    assert result.matched is True
    assert isinstance(result.condition, Expect)
    assert result.expect_match is not None
    assert result.expect_match.matched == "DONE>"
    assert writes == [("\n", False)]


def test_pseudo_terminal_wait_for_on_callback_buffer_can_answer_prompts() -> None:
    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            (
                "import sys\n"
                "sys.stdout.write('username:'); sys.stdout.flush()\n"
                "username = sys.stdin.readline().strip()\n"
                "sys.stdout.write('password:'); sys.stdout.flush()\n"
                "password = sys.stdin.readline().strip()\n"
                "sys.stdout.write(f'ok:{username}:{password}\\n'); sys.stdout.flush()\n"
            ),
        ],
        text=True,
    )

    def send_username(_match, buffer) -> WaitCallbackResult:
        buffer.write("alice\n")
        return WaitCallbackResult.CONTINUE_AND_DISARM

    def send_password(_match, buffer) -> WaitCallbackResult:
        buffer.write("secret\n")
        return WaitCallbackResult.CONTINUE_AND_DISARM

    result = process.wait_for(
        Expect("username:", on_callback=send_username),
        Expect("password:", on_callback=send_password),
        Expect("ok:alice:secret"),
        timeout=5.0,
    )

    assert result.matched is True
    assert isinstance(result.condition, Expect)
    assert result.expect_match is not None
    assert result.expect_match.matched == "ok:alice:secret"
    assert process.wait(timeout=5) == 0


def test_pseudo_terminal_wait_for_on_callback_propagates_keyboard_interrupt() -> None:
    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            (
                "import sys\n"
                "sys.stdout.write('username:'); sys.stdout.flush()\n"
                "sys.stdin.readline()\n"
            ),
        ],
        text=True,
    )

    def interrupt(_match, _buffer) -> WaitCallbackResult:
        raise KeyboardInterrupt

    try:
        with pytest.raises(KeyboardInterrupt):
            process.wait_for(
                Expect("username:", on_callback=interrupt),
                timeout=5.0,
            )
    finally:
        with contextlib.suppress(Exception):
            process.kill()


def test_pseudo_terminal_wait_for_expect_can_chain_next_expect() -> None:
    def send_username(_match, buffer) -> WaitCallbackResult:
        buffer.write("alice\n")
        return WaitCallbackResult.EXIT

    def send_password(_match, buffer) -> WaitCallbackResult:
        buffer.write("secret\n")
        return WaitCallbackResult.EXIT

    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            (
                "import sys\n"
                "sys.stdout.write('username:'); sys.stdout.flush()\n"
                "username = sys.stdin.readline().strip()\n"
                "sys.stdout.write('password:'); sys.stdout.flush()\n"
                "password = sys.stdin.readline().strip()\n"
                "sys.stdout.write(f'ok:{username}:{password}\\n'); sys.stdout.flush()\n"
            ),
        ],
        text=True,
        expect=[Expect("username:", on_callback=send_username)],
    )

    first = process.wait_for_expect(
        next_expect=Expect("password:", on_callback=send_password),
        timeout=5.0,
    )
    assert first.matched is True
    assert first.expect_match is not None
    assert first.expect_match.matched == "username:"

    second = process.wait_for_expect(
        next_expect=Expect("ok:alice:secret"),
        timeout=5.0,
    )
    assert second.matched is True
    assert second.expect_match is not None
    assert second.expect_match.matched == "password:"

    third = process.wait_for_expect(timeout=5.0)
    assert third.matched is True
    assert third.expect_match is not None
    assert third.expect_match.matched == "ok:alice:secret"
    assert process.wait(timeout=5) == 0


def test_pseudo_terminal_wait_for_expect_timeout_preserves_registered_expect(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    writes: list[tuple[str | bytes, bool]] = []
    stage = "first"
    fake_now = -0.01

    process = PseudoTerminalProcess(
        [sys.executable, "-c", "print('x')"],
        text=True,
        auto_run=False,
        expect=[Expect("ready>", action="\n")],
    )

    class FakeProc:
        pid = 1234
        exited = False

        def poll(self) -> int | None:
            return 0 if self.exited else None

        def close(self) -> None:
            return None

    fake_proc = FakeProc()
    process._proc = fake_proc  # type: ignore[assignment]
    monkeypatch.setattr(process, "_pump_native_output", lambda timeout, consume_all: None)
    monkeypatch.setattr(process, "_snapshot_output_history", lambda: ("", 0))

    def fake_time() -> float:
        nonlocal fake_now
        fake_now += 0.02
        return fake_now

    def snapshot_output_since(start: int) -> tuple[str, int]:
        if stage == "second" and start == 0:
            return ("ready>", 6)
        return ("", start)

    def fake_write(data: str | bytes, *, submit: bool = False) -> None:
        writes.append((data, submit))
        fake_proc.exited = True

    monkeypatch.setattr(pty_module.time, "time", fake_time)
    monkeypatch.setattr(process, "_snapshot_output_since", snapshot_output_since)
    process.write = fake_write  # type: ignore[method-assign]

    first = process.wait_for_expect(timeout=0.05)
    assert first.matched is False
    assert first.exit_reason == "timeout"

    stage = "second"
    second = process.wait_for_expect(timeout=5.0)
    assert second.matched is True
    assert second.expect_match is not None
    assert second.expect_match.matched == "ready>"
    assert writes == [("\n", False)]
    assert process._registered_expect_conditions == []


def test_pseudo_terminal_wait_for_expect_timeout_does_not_arm_next_expect(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    writes: list[tuple[str | bytes, bool]] = []
    stage = "first"
    fake_now = -0.01

    process = PseudoTerminalProcess(
        [sys.executable, "-c", "print('x')"],
        text=True,
        auto_run=False,
        expect=[Expect("username:", action="alice\n")],
    )

    class FakeProc:
        pid = 1234

        def poll(self) -> int | None:
            return None

        def close(self) -> None:
            return None

    process._proc = FakeProc()  # type: ignore[assignment]
    monkeypatch.setattr(process, "_pump_native_output", lambda timeout, consume_all: None)
    monkeypatch.setattr(process, "_snapshot_output_history", lambda: ("", 0))

    def fake_time() -> float:
        nonlocal fake_now
        fake_now += 0.02
        return fake_now

    def snapshot_output_since(start: int) -> tuple[str, int]:
        if stage == "second" and start == 0:
            return ("username:", 9)
        if stage == "third" and start == 0:
            return ("password:", 9)
        return ("", start)

    def fake_write(data: str | bytes, *, submit: bool = False) -> None:
        writes.append((data, submit))

    monkeypatch.setattr(pty_module.time, "time", fake_time)
    monkeypatch.setattr(process, "_snapshot_output_since", snapshot_output_since)
    process.write = fake_write  # type: ignore[method-assign]

    first = process.wait_for_expect(
        next_expect=Expect("password:", action="secret\n"),
        timeout=0.05,
    )
    assert first.matched is False
    assert first.exit_reason == "timeout"
    assert writes == []

    stage = "second"
    second = process.wait_for_expect(timeout=5.0)
    assert second.matched is True
    assert second.expect_match is not None
    assert second.expect_match.matched == "username:"
    assert writes == [("alice\n", False)]

    stage = "third"
    third = process.wait_for_expect(
        next_expect=Expect("password:", action="secret\n"),
        timeout=5.0,
    )
    assert third.matched is True
    assert third.expect_match is not None
    assert third.expect_match.matched == "password:"
    assert writes == [("alice\n", False), ("secret\n", False)]
    assert process._registered_expect_conditions == []


def test_pseudo_terminal_constructor_can_mix_expect_rule_and_registered_expect() -> None:
    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            (
                "import sys\n"
                "sys.stdout.write('bootstrap:'); sys.stdout.flush()\n"
                "sys.stdin.readline()\n"
                "sys.stdout.write('armed:'); sys.stdout.flush()\n"
                "sys.stdin.readline()\n"
            ),
        ],
        text=True,
        expect=[
            ExpectRule("bootstrap:", "boot\n"),
            Expect("armed:", action="armed\n"),
        ],
        expect_timeout=5.0,
    )

    result = process.wait_for_expect(timeout=5.0)
    assert result.matched is True
    assert result.expect_match is not None
    assert result.expect_match.matched == "armed:"
    assert process.wait(timeout=5) == 0


def test_pseudo_terminal_wait_for_callable_condition_does_not_block_expect() -> None:
    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            (
                "import sys, time\n"
                "time.sleep(0.02)\n"
                "sys.stdout.write('ready>'); sys.stdout.flush()\n"
                "sys.stdin.readline()\n"
            ),
        ],
        text=True,
    )

    def slow_false() -> bool:
        time.sleep(0.3)
        return False

    result = process.wait_for(
        Expect("ready>", action="\n"),
        slow_false,
        timeout=5.0,
    )

    assert result.matched is True
    assert isinstance(result.condition, Expect)
    assert result.expect_match is not None
    assert result.expect_match.matched == "ready>"
    assert process.wait(timeout=5) == 0
