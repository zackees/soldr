from __future__ import annotations

import os
import signal
import subprocess
import sys
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[1]
WRAPPER = REPO_ROOT / ".github" / "scripts" / "nextest_timeout_wrapper.py"
CONFIG = REPO_ROOT / ".config" / "nextest.toml"


def _start_wrapper(child: str, env: dict[str, str]) -> subprocess.Popen[str]:
    """Return a live wrapper so the test can inject SIGTERM before waiting."""

    return subprocess.Popen(  # pylint: disable=consider-using-with
        [sys.executable, str(WRAPPER), sys.executable, "-c", child],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=env,
    )


@pytest.mark.skipif(
    os.name != "posix", reason="Nextest has no Windows timeout grace hook"
)
def test_sigterm_dumps_threads_and_drains_child_output() -> None:
    child = """
import signal
import threading
import time

def stop(_signum, _frame):
    print("child output after termination", flush=True)
    raise SystemExit(0)

signal.signal(signal.SIGTERM, stop)
threading.Thread(target=lambda: time.sleep(60), daemon=True).start()
print("child output before timeout", flush=True)
while True:
    time.sleep(0.1)
"""
    env = os.environ.copy()
    env["SOLDR_NEXTEST_DISABLE_DEBUGGER"] = "1"
    env["SOLDR_NEXTEST_CHILD_EXIT_GRACE_SECS"] = "0.5"
    process = _start_wrapper(child, env)
    assert process.stdout is not None
    before_timeout = process.stdout.readline()
    process.send_signal(signal.SIGTERM)
    stdout_tail, stderr = process.communicate(timeout=15)
    stdout = before_timeout + stdout_tail

    assert process.returncode == 0
    assert "child output before timeout" in stdout
    assert "child output after termination" in stdout
    assert "nextest timeout: thread dump for pid" in stderr
    assert "nextest timeout: thread dump complete" in stderr
    assert "nextest timeout: stdout/stderr drained" in stderr
    if sys.platform.startswith("linux"):
        assert stderr.count("--- thread") >= 2
    else:
        assert "no platform thread dumper is available" in stderr


@pytest.mark.skipif(
    os.name != "posix", reason="Nextest has no Windows timeout grace hook"
)
def test_timeout_kills_descendant_that_retains_output_pipes() -> None:
    child = """
import signal
import subprocess
import sys
import time

grandchild = '''
import signal
import time

signal.signal(signal.SIGTERM, signal.SIG_IGN)
print("grandchild inherited output pipes", flush=True)
while True:
    time.sleep(1)
'''
subprocess.Popen([sys.executable, "-c", grandchild])

def stop(_signum, _frame):
    print("direct child exited after termination", flush=True)
    raise SystemExit(0)

signal.signal(signal.SIGTERM, stop)
print("direct child ready", flush=True)
while True:
    time.sleep(0.1)
"""
    env = os.environ.copy()
    env["SOLDR_NEXTEST_DISABLE_DEBUGGER"] = "1"
    process = _start_wrapper(child, env)
    assert process.stdout is not None
    ready_lines = [process.stdout.readline(), process.stdout.readline()]
    assert any("direct child ready" in line for line in ready_lines)
    assert any("grandchild inherited output pipes" in line for line in ready_lines)
    process.send_signal(signal.SIGTERM)
    stdout_tail, stderr = process.communicate(timeout=15)
    stdout = "".join(ready_lines) + stdout_tail

    assert process.returncode == 0
    assert "direct child ready" in stdout
    assert "direct child exited after termination" in stdout
    assert "grandchild inherited output pipes" in stdout
    assert "descendants retained output pipes; forcing exit" in stderr
    assert "nextest timeout: stdout/stderr drained" in stderr


@pytest.mark.skipif(
    os.name != "posix", reason="Nextest has no Windows timeout grace hook"
)
def test_signal_during_drain_kills_descendant_after_leader_already_exited() -> None:
    child = """
import signal
import subprocess
import sys

grandchild = '''
import signal
import time

signal.signal(signal.SIGTERM, signal.SIG_IGN)
print("orphan grandchild retained output pipes", flush=True)
while True:
    time.sleep(1)
'''
subprocess.Popen([sys.executable, "-c", grandchild])
print("direct child exiting normally", flush=True)
"""
    env = os.environ.copy()
    env["SOLDR_NEXTEST_DISABLE_DEBUGGER"] = "1"
    env["SOLDR_NEXTEST_CHILD_EXIT_GRACE_SECS"] = "0.5"
    process = _start_wrapper(child, env)
    assert process.stdout is not None
    ready_lines = [process.stdout.readline(), process.stdout.readline()]
    assert any("direct child exiting normally" in line for line in ready_lines)
    assert any(
        "orphan grandchild retained output pipes" in line for line in ready_lines
    )
    process.send_signal(signal.SIGTERM)
    stdout_tail, stderr = process.communicate(timeout=10)
    stdout = "".join(ready_lines) + stdout_tail

    assert process.returncode == 0
    assert "direct child exiting normally" in stdout
    assert "orphan grandchild retained output pipes" in stdout
    assert "descendants retained output pipes; forcing exit" in stderr
    assert "nextest timeout: stdout/stderr drained" in stderr


def test_nextest_config_wraps_unix_tests_with_a_bounded_grace_period() -> None:
    config = CONFIG.read_text(encoding="utf-8")
    assert 'experimental = ["wrapper-scripts"]' in config
    assert 'run-wrapper = "timeout-diagnostics"' in config
    assert 'platform = { target = "cfg(unix)" }' in config
    assert 'platform = { host = "cfg(unix)" }' not in config
    assert "[test-groups.soldr-runtime]" in config
    assert "binary(cli_broker_resurrection) + binary(cli_broker_routes)" in config
    assert config.count("test-group = 'soldr-runtime'") == 2
    for binary in (
        "agent_worktree_share",
        "cli_daemon_builds",
        "cli_daemon_flush_caches",
        "cli_daemon_lifecycle",
        "cli_daemon_single_instance",
        "cli_daemon_target_touch",
        "cli_daemon_tombstone",
        "daemon_cache_maintenance",
        "daemon_stall_harness",
        "session_multiprocess_smoke",
    ):
        assert f"binary({binary})" in config
    assert 'threads-required = "num-cpus"' in config
    assert config.count('grace-period = "30s"') == 5
