"""Tests for daemon PID tracking and RUNNING_PROCESS_SPAWNED_BY."""

import json
import os
import sys
import time
import unittest

import pytest

is_windows = sys.platform == "win32"


def _trampoline_available() -> bool:
    try:
        from running_process.daemon import _bundled_trampoline_path

        return _bundled_trampoline_path().exists()
    except Exception:
        return False


requires_trampoline = pytest.mark.skipif(
    not _trampoline_available(),
    reason="Trampoline binary not bundled in this build",
)


class TestPidTracking(unittest.TestCase):
    """Unit tests for PID tracking helpers — no trampoline required."""

    def setUp(self) -> None:
        # Save and clear the env var to isolate tests.
        self._saved = os.environ.pop("RUNNING_PROCESS_PIDS", None)

    def tearDown(self) -> None:
        if self._saved is not None:
            os.environ["RUNNING_PROCESS_PIDS"] = self._saved
        else:
            os.environ.pop("RUNNING_PROCESS_PIDS", None)

    def test_register_single_pid(self):
        from running_process.daemon import _register_daemon_pid, get_tracked_daemon_pids

        _register_daemon_pid(1234)
        self.assertEqual(get_tracked_daemon_pids(), [1234])

    def test_register_multiple_pids(self):
        from running_process.daemon import _register_daemon_pid, get_tracked_daemon_pids

        _register_daemon_pid(100)
        _register_daemon_pid(200)
        _register_daemon_pid(300)
        self.assertEqual(get_tracked_daemon_pids(), [100, 200, 300])

    def test_empty_when_unset(self):
        from running_process.daemon import get_tracked_daemon_pids

        self.assertEqual(get_tracked_daemon_pids(), [])


@requires_trampoline
class TestSpawnDaemonPidTracking(unittest.TestCase):
    """Integration tests — spawn a real daemon and verify PID tracking."""

    _daemon_name: str = ""

    def _unique_name(self, label: str) -> str:
        self._daemon_name = f"test-pid-{label}-{os.getpid()}"
        return self._daemon_name

    def setUp(self) -> None:
        self._saved = os.environ.pop("RUNNING_PROCESS_PIDS", None)

    def tearDown(self) -> None:
        if self._daemon_name:
            from running_process.daemon import cleanup_runtime

            cleanup_runtime(self._daemon_name)
            self._daemon_name = ""
        if self._saved is not None:
            os.environ["RUNNING_PROCESS_PIDS"] = self._saved
        else:
            os.environ.pop("RUNNING_PROCESS_PIDS", None)

    def test_daemon_pid_registered(self):
        """spawn_daemon registers the daemon PID in RUNNING_PROCESS_PIDS."""
        from running_process.daemon import get_tracked_daemon_pids, spawn_daemon

        name = self._unique_name("registered")
        handle = spawn_daemon(
            [sys.executable, "-c", "import time; time.sleep(5)"],
            name=name,
        )
        try:
            pids = get_tracked_daemon_pids()
            self.assertIn(handle.pid, pids)
        finally:
            self._kill_pid(handle.pid)

    def test_spawned_by_in_sidecar(self):
        """RUNNING_PROCESS_SPAWNED_BY is set in the sidecar env."""
        from running_process.daemon import spawn_daemon

        name = self._unique_name("spawnedby")
        handle = spawn_daemon(
            [sys.executable, "-c", "import time; time.sleep(5)"],
            name=name,
        )
        try:
            sidecar = handle.runtime_dir / f"{name}.daemon.json"
            data = json.loads(sidecar.read_text(encoding="utf-8"))
            spawned_by = data["env"]["RUNNING_PROCESS_SPAWNED_BY"]
            parent_pid, parent_name = spawned_by.split(":", 1)
            self.assertEqual(int(parent_pid), os.getpid())
            self.assertTrue(len(parent_name) > 0)
        finally:
            self._kill_pid(handle.pid)

    def test_daemon_receives_spawned_by(self):
        """The daemon process actually has RUNNING_PROCESS_SPAWNED_BY set."""
        from running_process.daemon import spawn_daemon

        name = self._unique_name("recvspawnedby")

        # The daemon writes its env to a file so we can verify.
        script = (
            "import os, json, pathlib, sys; "
            "pathlib.Path(sys.argv[1]).write_text("
            "json.dumps(dict(os.environ)), encoding='utf-8')"
        )

        from running_process.daemon import runtime_dir

        rd = runtime_dir(name)
        env_dump = rd / "env_dump.json"

        handle = spawn_daemon(
            [sys.executable, "-c", script, str(env_dump)],
            name=name,
        )
        try:
            deadline = time.monotonic() + 10
            while not env_dump.exists() and time.monotonic() < deadline:
                time.sleep(0.3)
            self.assertTrue(env_dump.exists(), "Daemon did not write env dump")
            daemon_env = json.loads(env_dump.read_text(encoding="utf-8"))
            self.assertIn("RUNNING_PROCESS_SPAWNED_BY", daemon_env)
            pid_str, _ = daemon_env["RUNNING_PROCESS_SPAWNED_BY"].split(":", 1)
            self.assertEqual(int(pid_str), os.getpid())
        finally:
            self._kill_pid(handle.pid)

    def test_running_process_vars_forwarded(self):
        """RUNNING_PROCESS_* vars from parent appear in daemon env."""
        from running_process.daemon import spawn_daemon

        name = self._unique_name("rpforward")

        script = (
            "import os, json, pathlib, sys; "
            "pathlib.Path(sys.argv[1]).write_text("
            "json.dumps(dict(os.environ)), encoding='utf-8')"
        )

        from running_process.daemon import runtime_dir

        rd = runtime_dir(name)
        env_dump = rd / "env_dump.json"

        # Set a marker RUNNING_PROCESS_* var in the parent.
        marker_key = "RUNNING_PROCESS_TEST_PHASE5_MARKER"
        old = os.environ.get(marker_key)
        os.environ[marker_key] = "phase5_value"
        try:
            handle = spawn_daemon(
                [sys.executable, "-c", script, str(env_dump)],
                name=name,
            )
            try:
                deadline = time.monotonic() + 10
                while not env_dump.exists() and time.monotonic() < deadline:
                    time.sleep(0.3)
                self.assertTrue(env_dump.exists(), "Daemon did not write env dump")
                daemon_env = json.loads(env_dump.read_text(encoding="utf-8"))
                self.assertEqual(daemon_env.get(marker_key), "phase5_value")
            finally:
                self._kill_pid(handle.pid)
        finally:
            if old is not None:
                os.environ[marker_key] = old
            else:
                os.environ.pop(marker_key, None)

    @staticmethod
    def _kill_pid(pid: int) -> None:
        try:
            if sys.platform == "win32":
                import subprocess

                subprocess.run(
                    ["taskkill", "/F", "/PID", str(pid)],
                    capture_output=True,
                    check=False,
                )
            else:
                import signal

                os.kill(pid, signal.SIGKILL)
        except OSError:
            pass
