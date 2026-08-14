#!/usr/bin/env python3
"""Wrap one Nextest test so SIGTERM dumps threads and drains its output."""

from __future__ import annotations

import ctypes
import os
import shutil
import signal
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import BinaryIO

DIAGNOSTIC_TIMEOUT_SECS = 12
CHILD_EXIT_GRACE_SECS = 8
CHILD_EXIT_GRACE_ENV = "SOLDR_NEXTEST_CHILD_EXIT_GRACE_SECS"


def _write_stderr(message: str) -> None:
    sys.stderr.write(message)
    sys.stderr.flush()


def _child_exit_grace() -> float:
    raw = os.environ.get(CHILD_EXIT_GRACE_ENV, "").strip()
    try:
        value = float(raw)
    except ValueError:
        return CHILD_EXIT_GRACE_SECS
    return value if value > 0 else CHILD_EXIT_GRACE_SECS


def _pump(source: BinaryIO, destination: BinaryIO) -> None:
    """Copy a child pipe through to Nextest until EOF."""

    try:
        try:
            read_available = getattr(source, "read1", source.read)
            while chunk := read_available(64 * 1024):
                destination.write(chunk)
                destination.flush()
        except (OSError, ValueError):
            pass
    finally:
        source.close()


def _linux_child_setup(parent_pid: int) -> None:
    """Isolate the child while guaranteeing it dies with the wrapper."""

    os.setsid()
    libc = ctypes.CDLL(None, use_errno=True)
    prctl = libc.prctl
    prctl.argtypes = [
        ctypes.c_int,
        ctypes.c_ulong,
        ctypes.c_ulong,
        ctypes.c_ulong,
        ctypes.c_ulong,
    ]
    prctl.restype = ctypes.c_int
    pr_set_pdeathsig = 1
    pr_set_ptracer = 0x59616D61
    if prctl(pr_set_pdeathsig, signal.SIGKILL, 0, 0, 0) != 0:
        os._exit(126)
    if prctl(pr_set_ptracer, parent_pid, 0, 0, 0) != 0:
        os.write(2, b"nextest timeout wrapper: ptrace authorization unavailable\n")
    if os.getppid() != parent_pid:
        os._exit(127)


def _posix_child_setup() -> None:
    os.setsid()


def _proc_thread_dump(pid: int) -> None:
    """Emit Linux thread state when a userspace debugger is unavailable."""

    task_root = Path("/proc") / str(pid) / "task"
    if not task_root.is_dir():
        _write_stderr(
            f"nextest timeout: /proc thread state unavailable for pid {pid}\n"
        )
        return
    for task in sorted(task_root.iterdir(), key=lambda path: int(path.name)):
        tid = task.name
        _write_stderr(f"\n--- thread {tid} ---\n")
        for name in ("comm", "wchan", "stack"):
            try:
                value = (task / name).read_text(encoding="utf-8", errors="replace")
            except OSError as error:
                value = f"<unavailable: {error}>\n"
            _write_stderr(f"{name}:\n{value}")
        try:
            status = (task / "status").read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        selected = [
            line
            for line in status.splitlines()
            if line.startswith(("Name:", "State:", "Tgid:", "Pid:", "PPid:"))
        ]
        _write_stderr("status:\n" + "\n".join(selected) + "\n")


def dump_threads(pid: int) -> None:
    """Dump userspace stacks when possible, then fall back to thread state."""

    _write_stderr(f"\n=== nextest timeout: thread dump for pid {pid} ===\n")
    debugger = (
        None
        if os.environ.get("SOLDR_NEXTEST_DISABLE_DEBUGGER")
        else shutil.which("gdb")
    )
    if debugger and sys.platform.startswith("linux"):
        try:
            completed = subprocess.run(
                [
                    debugger,
                    "--quiet",
                    "--batch",
                    "--nx",
                    "-ex",
                    "set pagination off",
                    "-ex",
                    "thread apply all backtrace full",
                    "-p",
                    str(pid),
                ],
                check=False,
                stdout=sys.stderr,
                stderr=subprocess.STDOUT,
                timeout=DIAGNOSTIC_TIMEOUT_SECS,
            )
            if completed.returncode == 0:
                _write_stderr(
                    "=== nextest timeout: debugger thread dump complete ===\n"
                )
                return
            _write_stderr(
                f"nextest timeout: gdb exited {completed.returncode}; using /proc fallback\n"
            )
        except (OSError, subprocess.TimeoutExpired) as error:
            _write_stderr(
                f"nextest timeout: gdb failed ({error}); using /proc fallback\n"
            )
    if sys.platform.startswith("linux"):
        _proc_thread_dump(pid)
    else:
        _write_stderr("nextest timeout: no platform thread dumper is available\n")
    _write_stderr("=== nextest timeout: thread dump complete ===\n")


def _signal_child_tree(child: subprocess.Popen[bytes], signum: int) -> None:
    if os.name == "posix":
        try:
            os.killpg(child.pid, signum)
        except ProcessLookupError:
            pass
    elif child.poll() is None:
        child.terminate()


def run(command: list[str]) -> int:
    """Run one test command and preserve all output around timeout shutdown."""

    parent_pid = os.getpid()
    child_exit_grace = _child_exit_grace()
    preexec_fn = None
    creationflags = 0
    if sys.platform.startswith("linux"):

        def linux_preexec() -> None:
            _linux_child_setup(parent_pid)

        preexec_fn = linux_preexec
    elif os.name == "posix":
        preexec_fn = _posix_child_setup
    elif os.name == "nt":
        creationflags = subprocess.CREATE_NEW_PROCESS_GROUP

    # This dedicated wrapper is single-threaded when Popen runs; pump threads
    # start only after the child exists, avoiding preexec_fn's thread deadlock
    # hazard. Linux needs that hook to install setsid/PDEATHSIG/ptrace policy.
    # Waiting and pipe closure are explicitly supervised below, so ownership
    # intentionally spans the whole run instead of a Popen context block.
    # pylint: disable-next=consider-using-with,subprocess-popen-preexec-fn
    child = subprocess.Popen(
        command,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        preexec_fn=preexec_fn,
        creationflags=creationflags,
    )
    assert child.stdout is not None and child.stderr is not None
    pumps = [
        threading.Thread(
            target=_pump, args=(child.stdout, sys.stdout.buffer), daemon=True
        ),
        threading.Thread(
            target=_pump, args=(child.stderr, sys.stderr.buffer), daemon=True
        ),
    ]
    for pump in pumps:
        pump.start()

    termination_requested = False
    termination_started: float | None = None

    def handle_termination(signum: int, _frame: object) -> None:
        nonlocal termination_requested, termination_started
        if termination_requested:
            return
        termination_requested = True
        if signum == signal.SIGTERM:
            dump_threads(child.pid)
        termination_started = time.monotonic()
        _signal_child_tree(child, signum)

    if os.name == "posix":
        signal.signal(signal.SIGTERM, handle_termination)
        signal.signal(signal.SIGINT, handle_termination)

    forced = False
    while child.poll() is None:
        if (
            termination_started is not None
            and time.monotonic() - termination_started >= child_exit_grace
        ):
            _write_stderr("nextest timeout: child ignored termination; forcing exit\n")
            _signal_child_tree(child, signal.SIGKILL)
            forced = True
            break
        time.sleep(0.05)
    returncode = child.wait()
    while any(pump.is_alive() for pump in pumps):
        if termination_started is not None:
            remaining = child_exit_grace - (time.monotonic() - termination_started)
            if remaining <= 0:
                if not forced:
                    _write_stderr(
                        "nextest timeout: descendants retained output pipes; forcing exit\n"
                    )
                    _signal_child_tree(child, signal.SIGKILL)
                break
            join_timeout = min(0.1, remaining)
        else:
            join_timeout = 0.1
        for pump in pumps:
            pump.join(timeout=join_timeout)
    if termination_started is not None:
        for pump in pumps:
            pump.join(timeout=2)
        if any(pump.is_alive() for pump in pumps):
            child.stdout.close()
            child.stderr.close()
            _write_stderr(
                "=== nextest timeout: output drain incomplete after SIGKILL ===\n"
            )
        else:
            _write_stderr("=== nextest timeout: stdout/stderr drained ===\n")
    return returncode


def main(argv: list[str] | None = None) -> int:
    command = list(sys.argv[1:] if argv is None else argv)
    if not command:
        _write_stderr("nextest timeout wrapper: missing test command\n")
        return 2
    return run(command)


if __name__ == "__main__":
    raise SystemExit(main())
