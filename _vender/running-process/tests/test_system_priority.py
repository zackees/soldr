from __future__ import annotations

import contextlib
import gc
import os
import subprocess
import sys
import threading
import time
import warnings
from pathlib import Path

import pytest

from running_process import (
    CpuPriority,
    RunningProcess,
    RunningProcessManagerSingleton,
)
from running_process.process_utils import get_process_tree_info, kill_process_tree
from tests.process_helpers import (
    WINDOWS_BELOW_NORMAL_PRIORITY_CLASS,
    wait_for_pid_exit,
    windows_priority_class_script,
)

live = pytest.mark.live
skip_unless_github_actions = pytest.mark.skipif(
    os.environ.get("GITHUB_ACTIONS", "").lower() != "true",
    reason="requires GitHub Actions runner",
)
pytestmark = [live, skip_unless_github_actions]


def test_manager_unregisters_after_wait() -> None:
    before = len(RunningProcessManagerSingleton.list_active())
    RunningProcess.run([sys.executable, "-c", "print('manager')"], capture_output=True)
    after = len(RunningProcessManagerSingleton.list_active())
    assert before == after


def test_running_process_manager_register_normalizes_pathlike_cwd(monkeypatch) -> None:
    from running_process.running_process_manager import RunningProcessManager

    seen: dict[str, object] = {}

    def fake_register(pid: int, kind: str, command: str, cwd: str | None) -> None:
        seen["pid"] = pid
        seen["kind"] = kind
        seen["command"] = command
        seen["cwd"] = cwd

    monkeypatch.setattr(
        "running_process.running_process_manager.native_register_process",
        fake_register,
    )

    proc = type(
        "FakeProc",
        (),
        {
            "pid": 123,
            "command": ["python", "-m", "ci.test"],
            "cwd": Path("C:/tmp/example"),
            "use_pty": False,
        },
    )()

    RunningProcessManager().register(proc)

    assert seen == {
        "pid": 123,
        "kind": "subprocess",
        "command": "python -m ci.test",
        "cwd": str(Path("C:/tmp/example")),
    }


def test_manager_does_not_hold_strong_reference_to_abandoned_process() -> None:
    before = len(RunningProcessManagerSingleton.list_active())
    process = RunningProcess([sys.executable, "-c", "import time; time.sleep(10)"])
    assert len(RunningProcessManagerSingleton.list_active()) == before + 1

    pid = process.pid
    del process
    gc.collect()

    deadline = time.time() + 3.0
    while time.time() < deadline:
        if len(RunningProcessManagerSingleton.list_active()) == before:
            break
        gc.collect()
        time.sleep(0.05)

    assert len(RunningProcessManagerSingleton.list_active()) == before
    if pid is not None:
        assert wait_for_pid_exit(pid, 3.0, before_sleep=gc.collect)


def test_process_utils_handle_invalid_pid() -> None:
    assert "Could not get process info" in get_process_tree_info(999999)
    kill_process_tree(999999)


def test_process_utils_describe_current_process() -> None:
    info = get_process_tree_info(os.getpid())
    assert f"Process {os.getpid()}" in info
    assert "Status:" in info


def test_child_python_env_defaults_to_utf8_replace() -> None:
    process = RunningProcess(
        [
            sys.executable,
            "-c",
            (
                "import os, sys; "
                "print(os.environ.get('PYTHONUTF8', '')); "
                "print(os.environ.get('PYTHONUNBUFFERED', '')); "
                "print(sys.stdout.encoding)"
            ),
        ]
    )
    process.wait()
    assert process.stdout.splitlines() == ["1", "1", "utf-8"]


def test_running_process_can_set_positive_nice() -> None:
    if sys.platform == "win32":
        script = windows_priority_class_script()
        process = RunningProcess([sys.executable, "-c", script], nice=5)
        process.wait()
        assert int(process.stdout) == WINDOWS_BELOW_NORMAL_PRIORITY_CLASS
        return

    process = RunningProcess(
        [sys.executable, "-c", "import os; print(os.nice(0))"],
        nice=5,
    )
    process.wait()
    assert int(process.stdout) >= 5


def test_running_process_accepts_platform_neutral_priority_enum() -> None:
    if sys.platform == "win32":
        script = windows_priority_class_script()
        process = RunningProcess([sys.executable, "-c", script], nice=CpuPriority.LOW)
        process.wait()
        assert int(process.stdout) == WINDOWS_BELOW_NORMAL_PRIORITY_CLASS
        return

    process = RunningProcess(
        [sys.executable, "-c", "import os; print(os.nice(0))"],
        nice=CpuPriority.LOW,
    )
    process.wait()
    assert int(process.stdout) >= 5


def test_running_process_rejects_invalid_nice_type() -> None:
    with pytest.raises(TypeError, match="nice must be an int, CpuPriority, or None"):
        RunningProcess([sys.executable, "-c", "print('x')"], auto_run=False, nice="low")  # type: ignore[arg-type]


def test_process_utils_reports_child_processes() -> None:
    script = """
import subprocess
import sys
import time

child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(10)"])
print(child.pid, flush=True)
time.sleep(10)
"""
    parent = subprocess.Popen(
        [sys.executable, "-c", script],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        assert parent.stdout is not None
        child_pid_line = parent.stdout.readline().strip()
        assert child_pid_line
        child_pid = int(child_pid_line)

        info = get_process_tree_info(parent.pid)
        assert f"Process {parent.pid}" in info
        assert f"Child {child_pid}" in info
    finally:
        kill_process_tree(parent.pid)
        with contextlib.suppress(Exception):
            parent.wait(timeout=2)


def test_kill_process_tree_kills_parent_and_child() -> None:
    script = """
import subprocess
import sys
import time

child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(10)"])
print(child.pid, flush=True)
time.sleep(10)
"""
    parent = subprocess.Popen(
        [sys.executable, "-c", script],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    assert parent.stdout is not None
    child_pid_line = parent.stdout.readline().strip()
    assert child_pid_line
    child_pid = int(child_pid_line)

    kill_process_tree(parent.pid)

    parent.wait(timeout=3)
    assert wait_for_pid_exit(parent.pid, 3.0)
    assert wait_for_pid_exit(child_pid, 3.0)


def test_running_process_force_killed_parent_reaps_child() -> None:
    if sys.platform != "win32":
        pytest.skip("Windows-specific parent crash behavior")

    script = """
import sys
import time
from running_process import RunningProcess

process = RunningProcess([sys.executable, "-c", "import time; time.sleep(2)"])
print(process.pid, flush=True)
time.sleep(2)
"""
    owner = subprocess.Popen(
        [sys.executable, "-c", script],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        assert owner.stdout is not None
        child_pid = int(owner.stdout.readline().strip())

        owner.kill()
        owner.wait(timeout=5)

        assert wait_for_pid_exit(child_pid, 5.0)
    finally:
        with contextlib.suppress(Exception):
            owner.kill()
        with contextlib.suppress(Exception):
            owner.wait(timeout=1)


def test_manager_dump_active_reports_process() -> None:
    process = RunningProcess([sys.executable, "-c", "import time; time.sleep(1)"])
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        RunningProcessManagerSingleton.dump_active()
    process.kill()
    assert any("STUCK SUBPROCESS COMMANDS" in str(item.message) for item in caught)


def test_manager_dump_active_reports_empty_state() -> None:
    for process in RunningProcessManagerSingleton.list_active():
        process.kill()
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        RunningProcessManagerSingleton.dump_active()
    assert any(
        "NO ACTIVE SUBPROCESSES DETECTED" in str(item.message) for item in caught
    )


def test_allows_child_ctrl_c_false_child_does_not_see_sigint() -> None:
    """When allows_child_ctrl_c_interruption=False, send_interrupt kills the child
    but the child's own SIGINT handler never fires."""
    process = RunningProcess(
        [
            sys.executable,
            "-c",
            (
                "import signal, time, sys\n"
                "got_sigint = False\n"
                "def handler(sig, frame):\n"
                "    global got_sigint\n"
                "    got_sigint = True\n"
                "    print('child-saw-sigint', flush=True)\n"
                "signal.signal(signal.SIGINT, handler)\n"
                "print('ready', flush=True)\n"
                "time.sleep(2)\n"
            ),
        ],
        allows_child_ctrl_c_interruption=False,
        timeout=10,
    )
    assert process.get_next_stdout_line(timeout=5) == "ready"
    # The parent kills the child — child never sees SIGINT
    process.kill()
    # Ensure child did not print "child-saw-sigint"
    remaining = process.stdout
    assert "child-saw-sigint" not in remaining


def test_allows_child_ctrl_c_false_process_group_isolation() -> None:
    """When allows_child_ctrl_c_interruption=False, the child is in its own
    process group and send_interrupt still works to explicitly interrupt it."""
    process = RunningProcess(
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
        allows_child_ctrl_c_interruption=False,
        timeout=10,
    )
    assert process.get_next_stdout_line(timeout=5) == "ready"
    # send_interrupt still works (explicit API call)
    process.send_interrupt()
    with pytest.raises(KeyboardInterrupt):
        process.wait()


def test_allows_child_ctrl_c_false_wait_kills_on_keyboard_interrupt() -> None:
    """When allows_child_ctrl_c_interruption=False, KeyboardInterrupt during
    wait() kills the child before re-raising."""
    process = RunningProcess(
        [
            sys.executable,
            "-c",
            "import time; print('ready', flush=True); time.sleep(2)",
        ],
        allows_child_ctrl_c_interruption=False,
        timeout=10,
    )
    assert process.get_next_stdout_line(timeout=5) == "ready"

    def trigger_interrupt() -> None:
        time.sleep(0.2)
        process.send_interrupt()

    worker = threading.Thread(target=trigger_interrupt, daemon=True)
    worker.start()

    with pytest.raises(KeyboardInterrupt):
        process.wait()
    worker.join(timeout=5)
    # Process should be dead — killed by the wait() handler
    assert process.returncode is not None
