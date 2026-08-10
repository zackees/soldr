from __future__ import annotations

import signal
import subprocess
import sys
import time

import pytest

from running_process import InteractiveMode, ProcessAbnormalExit, RunningProcess


def _interruptible_python_script() -> str:
    return (
        "import signal\n"
        "import sys\n"
        "import time\n"
        "print('ready', flush=True)\n"
        "def _handle(sig, frame):\n"
        "    print(f'caught:{sig}', flush=True)\n"
        "    raise KeyboardInterrupt\n"
        "signal.signal(signal.SIGINT, _handle)\n"
        "try:\n"
        "    time.sleep(2)\n"
        "except KeyboardInterrupt:\n"
        "    print('child-keyboard-interrupt', flush=True)\n"
        "    raise\n"
    )


@pytest.mark.live
def test_live_pipe_interrupt_raises_keyboard_interrupt() -> None:
    creationflags = (
        getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0) if sys.platform == "win32" else None
    )
    process = RunningProcess(
        [sys.executable, "-c", _interruptible_python_script()],
        creationflags=creationflags,
        timeout=2,
    )
    assert process.get_next_stdout_line(timeout=5) == "ready"
    process.send_interrupt()
    with pytest.raises(KeyboardInterrupt):
        process.wait()


@pytest.mark.live
def test_live_interactive_isolated_interrupt_raises_keyboard_interrupt() -> None:
    process = RunningProcess.interactive(
        [sys.executable, "-c", _interruptible_python_script()],
        mode=InteractiveMode.CONSOLE_ISOLATED,
    )
    time.sleep(0.5)
    process.send_interrupt()
    with pytest.raises(KeyboardInterrupt):
        process.wait(timeout=5)


@pytest.mark.live
def test_live_abnormal_exit_can_raise_after_force_kill() -> None:
    process = RunningProcess(
        [sys.executable, "-c", "import time; print('ready', flush=True); time.sleep(2)"],
        timeout=5,
    )
    assert process.get_next_stdout_line(timeout=5) == "ready"
    process.kill()
    with pytest.raises(ProcessAbnormalExit) as exc_info:
        process.wait(raise_on_abnormal_exit=True)

    assert exc_info.value.status.abnormal is True
    if sys.platform != "win32":
        assert exc_info.value.status.signal_number == signal.SIGKILL
        assert exc_info.value.status.possible_oom is True
    else:
        assert exc_info.value.status.returncode != 0
