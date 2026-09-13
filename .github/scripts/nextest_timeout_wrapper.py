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
# Truthy (`1`/`true`/`yes`/`on`) keeps a test's private TMPDIR for inspection.
KEEP_TMPDIR_ENV = "SOLDR_NEXTEST_KEEP_TMPDIR"
# soldr#3195: every test process refuses toolchain downloads (see
# crates/soldr-core/src/core/toolchain_install_tripwire.rs).
FORBID_TOOLCHAIN_INSTALL_ENV = "SOLDR_TEST_FORBID_TOOLCHAIN_INSTALL"


# How long to block waiting for the child before looping to re-check state.
#
# soldr#3144/#3138: this loop used to be `while child.poll() is None: time.sleep(0.05)`,
# which put a hard ~50 ms floor under EVERY test on Linux -- the wrapper runs
# for all of them (`filter = "all()"` in `.config/nextest.toml`). The floor was
# visible in CI: across 2,023 timed tests in one gate run the FASTEST was
# 0.066 s and not one landed under 0.06 s, for a suite where ~1,700 tests are
# sub-0.2 s unit tests.
#
# `Popen.wait(timeout=...)` returns the instant the child exits and, on POSIX,
# backs off exponentially from 0.5 ms rather than sleeping a flat 50 ms, so a
# short-lived test is reaped in about a millisecond.
#
# Why not `wait(timeout=None)`, which blocks in `waitpid` with no polling at
# all: PEP 475 restarts the syscall after a signal handler runs, so once
# `handle_termination` fires we would go straight back to blocking forever and
# never reach the `child_exit_grace` force-kill below. The bounded slice is what
# keeps that escape hatch reachable. While terminating, the slice narrows to the
# remaining grace so the deadline stays exact.
_IDLE_WAIT_SLICE_SECONDS = 1.0


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


def _private_tmpdir() -> str | None:
    """Create this test's private TMPDIR, or return None to leave TMPDIR alone.

    soldr#3079: about 420 integration-test call sites create a uniquely named
    directory under TMPDIR (`unique_temp_dir`) and never remove it. One
    workstation held 956 of them, 15 GiB. Every Unix test runs through this
    wrapper, so giving each test its own TMPDIR and removing it afterwards
    reclaims all of them at the source without touching a call site.

    Linux only: macOS's TMPDIR is already long and its `sun_path` limit is 104
    bytes, so extra depth there risks the Unix-socket endpoints tests bind
    under TMPDIR. The name is kept short (`snt<pid hex>`) for the same reason.
    """

    if not sys.platform.startswith("linux"):
        return None
    base = os.environ.get("TMPDIR") or "/tmp"
    stem = f"snt{os.getpid():x}"
    for attempt in range(16):
        path = os.path.join(base, stem if attempt == 0 else f"{stem}-{attempt}")
        try:
            os.mkdir(path, 0o700)
        except FileExistsError:
            continue
        except OSError:
            return None
        return path
    return None


def _remove_private_tmpdir(path: str | None) -> None:
    """Best-effort removal of a private TMPDIR once its test has exited."""

    if path is None:
        return
    keep = os.environ.get(KEEP_TMPDIR_ENV, "").strip().lower()
    if keep in {"1", "true", "yes", "on"}:
        return
    shutil.rmtree(path, ignore_errors=True)


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
    private_tmpdir = _private_tmpdir()
    child_env = dict(os.environ)
    if private_tmpdir is not None:
        child_env["TMPDIR"] = private_tmpdir
    # soldr#3195: a test must never download a Rust toolchain. Arm soldr's own
    # install tripwire, and stop rustup proxies from auto-installing a missing
    # toolchain, which soldr cannot see. An explicit value from the caller wins,
    # so one deliberate network run can still opt out.
    child_env.setdefault(FORBID_TOOLCHAIN_INSTALL_ENV, "1")
    child_env.setdefault("RUSTUP_AUTO_INSTALL", "0")
    try:
        # pylint: disable-next=consider-using-with,subprocess-popen-preexec-fn
        child = subprocess.Popen(
            command,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=child_env,
            preexec_fn=preexec_fn,
            creationflags=creationflags,
        )
    except BaseException:
        _remove_private_tmpdir(private_tmpdir)
        raise
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
    while True:
        if termination_started is not None:
            remaining = child_exit_grace - (time.monotonic() - termination_started)
            if remaining <= 0:
                _write_stderr(
                    "nextest timeout: child ignored termination; forcing exit\n"
                )
                _signal_child_tree(child, signal.SIGKILL)
                forced = True
                break
            wait_slice = remaining
        else:
            wait_slice = _IDLE_WAIT_SLICE_SECONDS
        try:
            child.wait(timeout=wait_slice)
            break
        except subprocess.TimeoutExpired:
            continue
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
    _remove_private_tmpdir(private_tmpdir)
    return returncode


def main(argv: list[str] | None = None) -> int:
    command = list(sys.argv[1:] if argv is None else argv)
    if not command:
        _write_stderr("nextest timeout wrapper: missing test command\n")
        return 2
    return run(command)


if __name__ == "__main__":
    raise SystemExit(main())
