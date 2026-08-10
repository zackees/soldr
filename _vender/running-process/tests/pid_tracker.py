"""Test-scoped PID tracker to prevent zombie process accumulation.

Every child PID spawned during tests is logged to an append-only file.
After each test, any PIDs still alive are force-killed and the test is
marked as failed.  An atexit handler provides a last-resort sweep.
"""

from __future__ import annotations

import atexit
import os
import signal
import sys
import time
from contextlib import suppress
from pathlib import Path

_LOG_DIR = Path(__file__).resolve().parent.parent / "logs"
_PID_LOG = _LOG_DIR / "test-spawned-pids.log"
_SELF_PID = os.getpid()


def _ensure_log_dir() -> None:
    _LOG_DIR.mkdir(parents=True, exist_ok=True)


def reset_log() -> None:
    """Truncate the PID log at the start of a test session."""
    _ensure_log_dir()
    _PID_LOG.write_text("", encoding="utf-8")


def record_pid(pid: int) -> None:
    """Append a PID to the log file (thread-safe via OS-level append)."""
    if pid == _SELF_PID or pid <= 0:
        return
    _ensure_log_dir()
    with open(_PID_LOG, "a", encoding="utf-8") as f:
        f.write(f"{pid}\n")
        f.flush()


def _read_pids() -> list[int]:
    """Read all recorded PIDs from the log."""
    if not _PID_LOG.exists():
        return []
    pids: list[int] = []
    with suppress(OSError):
        for line in _PID_LOG.read_text(encoding="utf-8").splitlines():
            line = line.strip()
            if line:
                with suppress(ValueError):
                    pids.append(int(line))
    return pids


def pid_alive(pid: int) -> bool:
    """Check if a PID is still running."""
    if pid <= 0 or pid == _SELF_PID:
        return False
    if sys.platform == "win32":
        import ctypes

        kernel32 = ctypes.windll.kernel32  # type: ignore[attr-defined]
        PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
        STILL_ACTIVE = 259
        handle = kernel32.OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, False, pid)
        if not handle:
            return False
        exit_code = ctypes.c_ulong()
        ok = kernel32.GetExitCodeProcess(handle, ctypes.byref(exit_code))
        kernel32.CloseHandle(handle)
        return bool(ok and exit_code.value == STILL_ACTIVE)
    else:
        try:
            os.kill(pid, 0)
            return True
        except ProcessLookupError:
            return False
        except PermissionError:
            return True


def _force_kill(pid: int) -> None:
    """Best-effort force-kill a single PID."""
    if pid <= 0 or pid == _SELF_PID:
        return
    with suppress(OSError, PermissionError):
        if sys.platform == "win32":
            import ctypes

            kernel32 = ctypes.windll.kernel32  # type: ignore[attr-defined]
            PROCESS_TERMINATE = 0x0001
            handle = kernel32.OpenProcess(PROCESS_TERMINATE, False, pid)
            if handle:
                kernel32.TerminateProcess(handle, 1)
                kernel32.CloseHandle(handle)
        else:
            os.kill(pid, signal.SIGKILL)


def reap_zombies(label: str = "") -> list[int]:
    """Kill all tracked PIDs that are still alive.

    Returns the list of PIDs that were killed.  Clears the PID log
    afterwards so dead/killed PIDs are not re-checked by later callers.
    """
    pids = _read_pids()
    killed: list[int] = []
    still_alive: list[int] = []
    for pid in pids:
        if pid_alive(pid):
            _force_kill(pid)
            killed.append(pid)
            # Check if the process actually died after the kill attempt.
            if pid_alive(pid):
                still_alive.append(pid)
    if killed and label:
        _log(f"[pid-tracker] {label}: killed {len(killed)} zombie(s): {killed}")
    # Rewrite the log with only stubbornly-alive PIDs so they can be
    # retried later; all other PIDs (dead or successfully killed) are
    # removed to prevent cascade failures in subsequent tests.
    reset_log()
    for pid in still_alive:
        record_pid(pid)
    return killed


def reap_with_retry(label: str = "", retries: int = 3, delay: float = 0.5) -> list[int]:
    """Kill zombies with retries for stubborn processes."""
    all_killed: list[int] = []
    for attempt in range(retries):
        killed = reap_zombies(label=f"{label} (attempt {attempt + 1})" if label else "")
        all_killed.extend(killed)
        if not killed:
            break
        if attempt < retries - 1:
            time.sleep(delay)
    return all_killed


def _log(message: str) -> None:
    with suppress(Exception):
        sys.stderr.write(f"{message}\n")
        sys.stderr.flush()


def install_atexit_handler() -> None:
    """Register a last-resort zombie killer for interpreter shutdown."""

    def _atexit_reap() -> None:
        reap_with_retry(label="atexit", retries=2, delay=0.3)

    atexit.register(_atexit_reap)
