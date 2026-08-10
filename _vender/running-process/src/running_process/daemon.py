"""Daemon spawning: directory layout, trampoline linking, sidecar JSON, and spawn_daemon API."""
from __future__ import annotations

import dataclasses
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path
from typing import Any


def _app_root() -> Path:
    """Return the platform-appropriate root for running-process app data.

    - Windows: %LOCALAPPDATA%/running-process
    - macOS:   ~/Library/Application Support/running-process
    - Linux:   ~/.local/share/running-process
    """
    if sys.platform == "win32":
        base = Path(os.environ.get("LOCALAPPDATA", Path.home() / "AppData" / "Local"))
    elif sys.platform == "darwin":
        base = Path.home() / "Library" / "Application Support"
    else:
        base = Path(os.environ.get("XDG_DATA_HOME", Path.home() / ".local" / "share"))
    return base / "running-process"


def assets_dir() -> Path:
    """Return the assets directory, creating it if needed."""
    d = _app_root() / "assets" / "trampoline"
    d.mkdir(parents=True, exist_ok=True)
    return d


def runtime_dir(name: str | None = None) -> Path:
    """Return the runtime directory, optionally for a specific daemon.

    If *name* is given, returns ``runtime/<name>/`` and creates it.
    """
    d = _app_root() / "runtime"
    if name is not None:
        d = d / name
    d.mkdir(parents=True, exist_ok=True)
    return d


def _bundled_trampoline_path() -> Path:
    """Return the path to the trampoline binary bundled inside the installed package."""
    assets = Path(__file__).resolve().parent / "assets"
    if sys.platform == "win32":
        return assets / "daemon-trampoline.exe"
    return assets / "daemon-trampoline"


def trampoline_source_path() -> Path:
    """Return the path to the trampoline binary in the app assets dir.

    On first call, copies from the bundled package assets to the app-level
    assets directory so that hard links from runtime/ always target a stable
    location on the same filesystem as ~/.running-process/.
    """
    ext = ".exe" if sys.platform == "win32" else ""
    dest = assets_dir() / f"daemon-trampoline{ext}"

    bundled = _bundled_trampoline_path()
    if not bundled.exists():
        raise FileNotFoundError(
            f"Bundled trampoline binary not found at {bundled}. "
            "Was the package built with trampoline support?"
        )

    # Copy if missing or outdated (different size).
    if not dest.exists() or dest.stat().st_size != bundled.stat().st_size:
        shutil.copy2(bundled, dest)
        # macOS: re-sign after copy to keep Gatekeeper happy.
        if sys.platform == "darwin":
            subprocess.run(
                ["codesign", "--force", "--sign", "-", str(dest)],
                check=False,
                capture_output=True,
            )

    return dest


def hard_link_trampoline(name: str) -> Path:
    """Hard-link the trampoline binary into the daemon's runtime directory.

    The link is named ``<name>`` (or ``<name>.exe`` on Windows) so the process
    shows up under that name in Task Manager / ps.

    Falls back to copy if hard-linking fails (e.g., cross-filesystem).
    On macOS copy fallback, re-signs the binary.

    Returns the path to the linked/copied trampoline.
    """
    ext = ".exe" if sys.platform == "win32" else ""
    dest_dir = runtime_dir(name)
    dest = dest_dir / f"{name}{ext}"

    if dest.exists():
        dest.unlink()

    source = trampoline_source_path()

    try:
        os.link(source, dest)
    except OSError:
        shutil.copy2(source, dest)
        if sys.platform == "darwin":
            subprocess.run(
                ["codesign", "--force", "--sign", "-", str(dest)],
                check=False,
                capture_output=True,
            )

    return dest


def write_sidecar(
    name: str,
    *,
    command: str,
    args: list[str] | None = None,
    cwd: str | Path | None = None,
    env: dict[str, str] | None = None,
    spawned_at_unix_ms: int | None = None,
    last_seen_unix_ms: int | None = None,
) -> Path:
    """Write the daemon sidecar JSON file next to the trampoline link.

    Returns the path to the written sidecar file.
    """
    dest_dir = runtime_dir(name)
    sidecar_path = dest_dir / f"{name}.daemon.json"

    data: dict[str, Any] = {"command": str(command)}
    if args:
        data["args"] = args
    if cwd is not None:
        data["cwd"] = str(cwd)
    if env is not None:
        data["env"] = env
    if spawned_at_unix_ms is not None:
        data["spawned_at_unix_ms"] = int(spawned_at_unix_ms)
    if last_seen_unix_ms is not None:
        data["last_seen_unix_ms"] = int(last_seen_unix_ms)

    sidecar_path.write_text(json.dumps(data, indent=2), encoding="utf-8")
    return sidecar_path


def cleanup_runtime(name: str) -> None:
    """Remove a daemon's runtime directory and all its contents."""
    d = _app_root() / "runtime" / name
    if d.exists():
        shutil.rmtree(d, ignore_errors=True)


# ---------------------------------------------------------------------------
# DaemonHandle
# ---------------------------------------------------------------------------


class DaemonOutputNotAvailableError(Exception):
    """Raised when attempting to read stdout/stderr from a daemon process."""


@dataclasses.dataclass
class DaemonHandle:
    """Handle returned by :func:`spawn_daemon`.

    The daemon is fire-and-forget — stdout/stderr are redirected to a log file,
    not piped to the caller.
    """

    pid: int
    name: str
    runtime_dir: Path
    log_path: Path | None

    def is_running(self) -> bool:
        """Return True if the daemon process is still alive."""
        if sys.platform == "win32":
            return self._is_running_win32()
        child_running = _posix_child_running(self.pid)
        if child_running is not None:
            return child_running
        try:
            os.kill(self.pid, 0)
        except OSError:
            return False
        state = _posix_process_state(self.pid)
        if state is None:
            state = _posix_ps_process_state(self.pid)
        return state != "Z"

    def _is_running_win32(self) -> bool:
        """Windows-specific liveness check using GetExitCodeProcess."""
        import ctypes

        kernel32 = ctypes.windll.kernel32  # type: ignore[attr-defined]
        process_query_limited_information = 0x1000
        still_active = 259
        handle = kernel32.OpenProcess(process_query_limited_information, False, self.pid)
        if not handle:
            return False
        try:
            exit_code = ctypes.c_ulong()
            if kernel32.GetExitCodeProcess(handle, ctypes.byref(exit_code)):
                return exit_code.value == still_active
            return False
        finally:
            kernel32.CloseHandle(handle)

    def read_stdout(self) -> str:
        """Always raises — daemon stdout is not piped to the caller."""
        msg = "Daemon stdout is not available. "
        if self.log_path and self.log_path.exists():
            msg += f"Check the log file at: {self.log_path}"
        else:
            msg += "No log file was configured."
        raise DaemonOutputNotAvailableError(msg)


def _posix_process_state(pid: int) -> str | None:
    """Return the /proc process state marker for *pid* when available."""
    if sys.platform == "win32":
        return None
    stat_path = Path(f"/proc/{pid}/stat")
    try:
        stat_text = stat_path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None
    close_paren = stat_text.rfind(")")
    if close_paren == -1 or close_paren + 2 >= len(stat_text):
        return None
    return stat_text[close_paren + 2]


def _posix_ps_process_state(pid: int) -> str | None:
    """Return the leading ``ps`` status marker for *pid* when available."""
    if sys.platform == "win32":
        return None
    try:
        result = subprocess.run(
            ["ps", "-o", "stat=", "-p", str(pid)],
            capture_output=True,
            text=True,
            check=False,
            timeout=5,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if result.returncode != 0:
        return None
    status = result.stdout.strip()
    if not status:
        return None
    return status[0]


def _posix_child_running(pid: int) -> bool | None:
    """Return child liveness via ``waitpid(WNOHANG)`` when *pid* is our child."""
    if sys.platform == "win32":
        return None
    try:
        waited_pid, _status = os.waitpid(pid, os.WNOHANG)
    except ChildProcessError:
        return None
    except OSError:
        return None
    if waited_pid == 0:
        return True
    if waited_pid == pid:
        return False
    return None


# ---------------------------------------------------------------------------
# Environment building
# ---------------------------------------------------------------------------

# Variables to always strip from the daemon environment.
# These leak venv, build tool, or dynamic linker state from the parent.
_STRIP_ENV_VARS: set[str] = {
    "VIRTUAL_ENV",
    "PYTHONHOME",
    "PYTHONPATH",
    "CONDA_DEFAULT_ENV",
    "CONDA_PREFIX",
    "CONDA_PYTHON_EXE",
    "CONDA_SHLVL",
    "_CE_CONDA",
    "_CE_M",
    "PKG_CONFIG_PATH",
    "CARGO_HOME",
    "RUSTUP_HOME",
}

# Prefixes for variables to strip (e.g., PIP_INDEX_URL, PIP_REQUIRE_VIRTUALENV).
_STRIP_ENV_PREFIXES: tuple[str, ...] = ("PIP_", "CONDA_")

# PATH components that indicate a venv or build-tool bin directory.
_VENV_PATH_MARKERS: tuple[str, ...] = (
    "/.venv/",
    "/venv/",
    "/virtualenv/",
    "\\venv\\",
    "\\.venv\\",
    "\\virtualenv\\",
    "/Scripts",
    "\\Scripts",
)


def _is_venv_path_component(component: str) -> bool:
    """Return True if a PATH component looks like a venv bin directory."""
    for marker in _VENV_PATH_MARKERS:
        if marker in component:
            return True
    # Also check for the exact VIRTUAL_ENV/bin pattern
    venv = os.environ.get("VIRTUAL_ENV", "")
    if venv and component.startswith(venv):
        return True
    return False


def _clean_path(raw_path: str) -> str:
    """Remove venv/build-tool entries from a PATH string."""
    sep = ";" if sys.platform == "win32" else ":"
    cleaned = [c for c in raw_path.split(sep) if c and not _is_venv_path_component(c)]
    return sep.join(cleaned)


def _platform_default_path() -> str:
    """Return a minimal platform-appropriate default PATH."""
    if sys.platform == "win32":
        sys_root = os.environ.get("SystemRoot", r"C:\Windows")
        return ";".join([
            os.path.join(sys_root, "system32"),
            sys_root,
            os.path.join(sys_root, "System32", "Wbem"),
            os.path.join(sys_root, "System32", "WindowsPowerShell", "v1.0"),
        ])
    return "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"


def build_daemon_env(
    caller_env: dict[str, str] | None = None,
) -> dict[str, str]:
    """Build a clean environment for a daemon process.

    1. Start with the current process environment.
    2. Strip venv/build-tool variables.
    3. Clean PATH to remove venv bin directories.
    4. Merge caller-specified overrides.
    5. Forward RUNNING_PROCESS_* variables from the parent.

    If the resulting PATH is empty after cleaning, use platform defaults.
    """
    # Start from current env.
    env = dict(os.environ)

    # Strip known venv/build-tool vars.
    for key in list(env):
        if key in _STRIP_ENV_VARS:
            del env[key]
        elif any(key.startswith(prefix) for prefix in _STRIP_ENV_PREFIXES):
            del env[key]

    # Also strip LD_LIBRARY_PATH / DYLD_LIBRARY_PATH on Unix.
    if sys.platform != "win32":
        env.pop("LD_LIBRARY_PATH", None)
        env.pop("DYLD_LIBRARY_PATH", None)

    # Clean PATH.
    raw_path = env.get("PATH", env.get("Path", ""))
    path_key = "Path" if "Path" in env else "PATH"
    cleaned = _clean_path(raw_path)
    if not cleaned:
        cleaned = _platform_default_path()
    env[path_key] = cleaned

    # Merge caller overrides.
    if caller_env:
        env.update(caller_env)

    # Forward RUNNING_PROCESS_* vars from the real parent environment.
    for key, value in os.environ.items():
        if key.startswith("RUNNING_PROCESS_"):
            env.setdefault(key, value)

    return env


# ---------------------------------------------------------------------------
# PID tracking
# ---------------------------------------------------------------------------

_RUNNING_PROCESS_PIDS_ENV = "RUNNING_PROCESS_PIDS"


def _register_daemon_pid(pid: int) -> None:
    """Append *pid* to the RUNNING_PROCESS_PIDS env var (comma-separated)."""
    current = os.environ.get(_RUNNING_PROCESS_PIDS_ENV, "")
    pids = [p.strip() for p in current.split(",") if p.strip()]
    pids.append(str(pid))
    os.environ[_RUNNING_PROCESS_PIDS_ENV] = ",".join(pids)


def get_tracked_daemon_pids() -> list[int]:
    """Return the list of daemon PIDs registered via RUNNING_PROCESS_PIDS."""
    raw = os.environ.get(_RUNNING_PROCESS_PIDS_ENV, "")
    return [int(p.strip()) for p in raw.split(",") if p.strip()]


# ---------------------------------------------------------------------------
# spawn_daemon
# ---------------------------------------------------------------------------

# Windows creation flags
_DETACHED_PROCESS = 0x0000_0008
_CREATE_NEW_PROCESS_GROUP = 0x0000_0200


def spawn_daemon(
    cmd: list[str] | str,
    *,
    name: str,
    cwd: str | Path | None = None,
    env: dict[str, str] | None = None,
    log_path: str | Path | None = None,
) -> DaemonHandle:
    """Spawn a daemon process via the binary trampoline.

    The daemon runs detached from the caller — stdin is /dev/null, stdout/stderr
    go to *log_path* (or are discarded if not specified).

    Parameters
    ----------
    cmd:
        The command to run (list or shell string).
    name:
        Process name — the trampoline binary is hard-linked under this name so
        the daemon appears with this name in ``ps`` / Task Manager.
    cwd:
        Working directory for the daemon (default: inherit caller).
    env:
        Extra environment variables to merge into the daemon's clean
        environment.  The daemon always starts with a clean env (venv and
        build-tool vars stripped).  Pass explicit overrides here.
    log_path:
        Path to a log file.  The parent opens the file in append mode before
        spawning so permission errors are reported synchronously.  If ``None``,
        stdout/stderr go to ``DEVNULL``.
    """
    # 1. Resolve command to (program, args).
    if isinstance(cmd, str):
        program = cmd
        args: list[str] = []
    else:
        program = cmd[0]
        args = list(cmd[1:])

    # 2. Build a clean daemon environment.
    daemon_env = build_daemon_env(caller_env=env)

    # 2b. Inject RUNNING_PROCESS_SPAWNED_BY so the daemon knows its parent.
    parent_pid = os.getpid()
    parent_name = Path(sys.argv[0]).stem if sys.argv else "unknown"
    daemon_env["RUNNING_PROCESS_SPAWNED_BY"] = f"{parent_pid}:{parent_name}"
    spawned_at_unix_ms = int(time.time() * 1000)

    # 3. Prepare runtime directory.
    rd = runtime_dir(name)

    # 4. Write sidecar JSON (always with explicit env so the trampoline
    #    env_clear()s and applies only what's in the sidecar).
    write_sidecar(
        name,
        command=program,
        args=args,
        cwd=cwd,
        env=daemon_env,
        spawned_at_unix_ms=spawned_at_unix_ms,
        last_seen_unix_ms=spawned_at_unix_ms,
    )

    # 5. Hard-link the trampoline.
    trampoline = hard_link_trampoline(name)

    # 6. Open log file if requested (parent opens for sync error reporting).
    if log_path is not None:
        log_path = Path(log_path)
        log_path.parent.mkdir(parents=True, exist_ok=True)
        log_fd = open(log_path, "a")
        stdout_target: Any = log_fd
        stderr_target: Any = log_fd
    else:
        log_fd = None
        stdout_target = subprocess.DEVNULL
        stderr_target = subprocess.DEVNULL

    # 7. Spawn the trampoline.
    try:
        kwargs: dict[str, Any] = {
            "stdin": subprocess.DEVNULL,
            "stdout": stdout_target,
            "stderr": stderr_target,
        }
        if sys.platform == "win32":
            kwargs["creationflags"] = _DETACHED_PROCESS | _CREATE_NEW_PROCESS_GROUP
        else:
            kwargs["start_new_session"] = True

        proc = subprocess.Popen([str(trampoline)], **kwargs)
    finally:
        # Close the log fd in the parent — the child inherited it.
        if log_fd is not None:
            log_fd.close()

    # 8. Write PID file.
    pid_file = rd / "daemon.pid"
    pid_file.write_text(str(proc.pid), encoding="utf-8")

    # 9. Register daemon PID in the parent's RUNNING_PROCESS_PIDS env var.
    _register_daemon_pid(proc.pid)

    return DaemonHandle(
        pid=proc.pid,
        name=name,
        runtime_dir=rd,
        log_path=log_path,
    )
