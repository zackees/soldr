from __future__ import annotations

import faulthandler
import os
import sys
import threading
from collections.abc import Iterator

import pytest

from tests.pid_tracker import (
    install_atexit_handler,
    reap_with_retry,
    reap_zombies,
    record_pid,
    reset_log,
)

_DEFAULT_TEST_TIMEOUT_SECONDS = 20.0
_IN_RUNNING_PROCESS_ENV = "IN_RUNNING_PROCESS"
_EXPECTED_IN_RUNNING_PROCESS = "running-process-cli"


def pytest_sessionstart(session: pytest.Session) -> None:
    del session
    actual = os.environ.get(_IN_RUNNING_PROCESS_ENV)
    if actual != _EXPECTED_IN_RUNNING_PROCESS:
        raise RuntimeError(
            "pytest must run under the running-process test entrypoint; "
            f"expected {_IN_RUNNING_PROCESS_ENV}={_EXPECTED_IN_RUNNING_PROCESS!r}, "
            f"got {actual!r}. Run pytest through the running-process CLI so it sets "
            "IN_RUNNING_PROCESS=running-process-cli and can auto-dump stacks when a "
            "test hangs. Do not manually inject IN_RUNNING_PROCESS in the agent or "
            "test command."
        )
    reset_log()
    install_atexit_handler()
    _install_pid_tracking_hooks()


def _install_pid_tracking_hooks() -> None:
    """Patch PseudoTerminalProcess.start and RunningProcess to log PIDs."""
    # PseudoTerminalProcess — wraps native PTY start
    from running_process.pty import PseudoTerminalProcess

    _orig_pty_start = PseudoTerminalProcess.start

    def _tracked_pty_start(self: PseudoTerminalProcess) -> None:
        _orig_pty_start(self)
        pid = self.pid
        if pid is not None:
            record_pid(pid)

    PseudoTerminalProcess.start = _tracked_pty_start  # type: ignore[method-assign]

    # RunningProcess — wraps native subprocess start
    from running_process.running_process import RunningProcess

    _orig_rp_start = RunningProcess.start

    def _tracked_rp_start(self: RunningProcess) -> None:
        _orig_rp_start(self)
        pid = getattr(self, "pid", None)
        if pid is not None:
            record_pid(pid)

    RunningProcess.start = _tracked_rp_start  # type: ignore[method-assign]


def pytest_sessionfinish(session: pytest.Session, exitstatus: int) -> None:
    """Kill any zombie processes left after the entire test session."""
    del session, exitstatus
    reap_with_retry(label="session-end", retries=3, delay=0.5)


def _crash_current_process_for_test_timeout(nodeid: str, timeout_seconds: float) -> None:
    message = f"\nCRASHED!!! {nodeid} exceeded {timeout_seconds:.0f}s\n"
    os.write(2, message.encode("utf-8", errors="replace"))
    try:
        from running_process._native import native_dump_rust_debug_traces

        rust_dump = native_dump_rust_debug_traces()
        if rust_dump.strip():
            os.write(2, b"\n[running-process rust debug trace]\n")
            os.write(2, rust_dump.encode("utf-8", errors="replace"))
    except Exception:
        pass
    # Last resort: kill all tracked zombies before crashing.
    reap_with_retry(label=f"timeout-crash:{nodeid}", retries=2, delay=0.2)
    crash_stream = sys.__stderr__
    crash_stream.flush()
    faulthandler.dump_traceback(file=crash_stream, all_threads=True)
    crash_stream.flush()
    os._exit(1)


@pytest.fixture(autouse=True)
def _per_test_process_watchdog(request: pytest.FixtureRequest) -> Iterator[None]:
    configured_timeout = os.environ.get("RUNNING_PROCESS_TEST_TIMEOUT_SECONDS")
    timeout_seconds = (
        float(configured_timeout) if configured_timeout else _DEFAULT_TEST_TIMEOUT_SECONDS
    )
    timer = threading.Timer(
        timeout_seconds,
        _crash_current_process_for_test_timeout,
        args=(request.node.nodeid, timeout_seconds),
    )
    timer.daemon = True
    timer.start()
    try:
        yield
    finally:
        timer.cancel()
        # Kill any child processes left behind by this test.
        killed = reap_zombies(label=request.node.nodeid)
        if killed:
            pytest.fail(
                f"Test left {len(killed)} zombie process(es): {killed}. "
                "All child processes must be cleaned up before the test ends.",
                pytrace=False,
            )


def pytest_collection_modifyitems(config: pytest.Config, items: list[pytest.Item]) -> None:
    if os.environ.get("RUNNING_PROCESS_LIVE_TESTS") == "1":
        return

    skip_live = pytest.mark.skip(
        reason="live tests require RUNNING_PROCESS_LIVE_TESTS=1"
    )
    for item in items:
        if "live" in item.keywords:
            item.add_marker(skip_live)
