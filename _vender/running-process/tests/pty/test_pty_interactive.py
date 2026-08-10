"""InteractiveProcess facade and interactive launch spec tests."""

from __future__ import annotations

import sys
import time

import pytest

from running_process import (
    InteractiveMode,
    RunningProcess,
)
from running_process.pty import interactive_launch_spec


def test_interactive_launch_specs_model_windows_modes() -> None:
    pty_spec = interactive_launch_spec(InteractiveMode.PSEUDO_TERMINAL)
    shared_spec = interactive_launch_spec(InteractiveMode.CONSOLE_SHARED)
    isolated_spec = interactive_launch_spec(InteractiveMode.CONSOLE_ISOLATED)

    assert pty_spec.uses_pty is True
    assert pty_spec.ctrl_c_owner == "child"
    assert pty_spec.restore_terminal is True

    assert shared_spec.uses_pty is False
    assert shared_spec.ctrl_c_owner == "shared"
    assert shared_spec.restore_terminal is False

    assert isolated_spec.uses_pty is False
    assert isolated_spec.ctrl_c_owner == "parent"
    assert isolated_spec.restore_terminal is True


def test_running_process_interactive_launches_console_mode() -> None:
    process = RunningProcess.interactive(
        [sys.executable, "-c", "print('interactive')"],
        mode=InteractiveMode.CONSOLE_SHARED,
    )
    assert process.wait(timeout=5) == 0


def test_interactive_kill_waits_for_exit() -> None:
    process = RunningProcess.interactive(
        [sys.executable, "-c", "import time; time.sleep(10)"],
        mode=InteractiveMode.CONSOLE_SHARED,
    )
    process.kill()
    assert process.poll() is not None


def test_interactive_wait_raises_keyboard_interrupt_on_sigint() -> None:
    process = RunningProcess.interactive(
        [
            sys.executable,
            "-c",
            (
                "import time\n"
                "print('ready', flush=True)\n"
                "try:\n"
                "    time.sleep(2)\n"
                "except KeyboardInterrupt:\n"
                "    raise\n"
            ),
        ],
        mode=InteractiveMode.CONSOLE_ISOLATED,
    )
    time.sleep(0.5)
    process.send_interrupt()
    with pytest.raises(KeyboardInterrupt):
        process.wait(timeout=2)


def test_running_process_exposes_interactive_launch_spec() -> None:
    spec = RunningProcess.interactive_launch_spec("console_isolated")
    assert spec.mode is InteractiveMode.CONSOLE_ISOLATED
    assert spec.ctrl_c_owner == "parent"
