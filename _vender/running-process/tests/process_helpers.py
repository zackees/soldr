from __future__ import annotations

import os
import sys
import time
from collections.abc import Callable
from pathlib import Path

WINDOWS_BELOW_NORMAL_PRIORITY_CLASS = 0x0000_4000
_WINDOWS_PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
_WINDOWS_STILL_ACTIVE = 259


def pid_exists(pid: int) -> bool:
    pid = int(pid)
    if sys.platform == "win32":
        import ctypes
        import ctypes.wintypes as wintypes

        kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel32.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
        kernel32.OpenProcess.restype = wintypes.HANDLE
        kernel32.GetExitCodeProcess.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.DWORD)]
        kernel32.GetExitCodeProcess.restype = wintypes.BOOL
        kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
        kernel32.CloseHandle.restype = wintypes.BOOL

        handle = kernel32.OpenProcess(_WINDOWS_PROCESS_QUERY_LIMITED_INFORMATION, False, pid)
        if not handle:
            return False
        exit_code = wintypes.DWORD()
        try:
            if not kernel32.GetExitCodeProcess(handle, ctypes.byref(exit_code)):
                return False
            return exit_code.value == _WINDOWS_STILL_ACTIVE
        finally:
            kernel32.CloseHandle(handle)

    if sys.platform.startswith("linux"):
        stat_path = Path(f"/proc/{pid}/stat")
        try:
            stat_fields = stat_path.read_text(encoding="utf-8").split()
        except FileNotFoundError:
            return False
        except OSError:
            return True
        if len(stat_fields) >= 3 and stat_fields[2] == "Z":
            return False
        return True

    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def wait_for_pid_exit(
    pid: int,
    timeout_seconds: float,
    *,
    before_sleep: Callable[[], None] | None = None,
) -> bool:
    deadline = time.time() + max(0.0, timeout_seconds)
    while time.time() < deadline and pid_exists(pid):
        if before_sleep is not None:
            before_sleep()
        time.sleep(0.05)
    return not pid_exists(pid)


def windows_priority_class_script(*, output_path: Path | None = None) -> str:
    lines = [
        "import ctypes",
        "import ctypes.wintypes as wintypes",
        "import os",
        "kernel32 = ctypes.WinDLL('kernel32', use_last_error=True)",
        "kernel32.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]",
        "kernel32.OpenProcess.restype = wintypes.HANDLE",
        "kernel32.GetPriorityClass.argtypes = [wintypes.HANDLE]",
        "kernel32.GetPriorityClass.restype = wintypes.DWORD",
        "kernel32.CloseHandle.argtypes = [wintypes.HANDLE]",
        "kernel32.CloseHandle.restype = wintypes.BOOL",
        "PROCESS_QUERY_LIMITED_INFORMATION = 0x1000",
        "handle = kernel32.OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, False, os.getpid())",
        "if not handle:",
        "    raise OSError(ctypes.get_last_error())",
        "try:",
        "    value = kernel32.GetPriorityClass(handle)",
        "    if not value:",
        "        raise OSError(ctypes.get_last_error())",
    ]
    if output_path is None:
        lines.append("    print(value, flush=True)")
    else:
        lines.extend(
            [
                "    from pathlib import Path",
                f"    Path(r'{output_path}').write_text(str(value), encoding='utf-8')",
            ]
        )
    lines.extend(
        [
            "finally:",
            "    kernel32.CloseHandle(handle)",
        ]
    )
    return "\n".join(lines)
