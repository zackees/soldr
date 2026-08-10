"""Tests for the daemon trampoline binary and Python daemon helpers."""
from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parent.parent


def _trampoline_binary() -> Path:
    """Return the path to the trampoline binary, preferring target/debug."""
    ext = ".exe" if sys.platform == "win32" else ""
    candidates = [
        ROOT / "target" / "debug" / f"daemon-trampoline{ext}",
        ROOT / "src" / "running_process" / "assets" / f"daemon-trampoline{ext}",
    ]
    for c in candidates:
        if not c.exists():
            continue
        try:
            # Probe that the binary actually executes on this platform: a
            # mounted target/ dir can hold a foreign-libc ELF (e.g. glibc
            # binary exec'd in a musl container fails ENOENT on its
            # interpreter).
            subprocess.run([str(c)], capture_output=True, timeout=30, check=False)
        except OSError:
            continue
        return c
    raise unittest.SkipTest(
        "daemon-trampoline binary not built; run `cargo build --bin daemon-trampoline`"
    )


class TestTrampolineBinary(unittest.TestCase):
    """Test the daemon-trampoline Rust binary directly."""

    def setUp(self) -> None:
        self.trampoline = _trampoline_binary()
        self.tmpdir = Path(tempfile.mkdtemp(prefix="trampoline-test-"))

    def tearDown(self) -> None:
        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def _setup_trampoline(self, name: str, sidecar: dict) -> Path:
        """Copy trampoline to tmpdir with a custom name and write its sidecar."""
        ext = ".exe" if sys.platform == "win32" else ""
        dest = self.tmpdir / f"{name}{ext}"
        shutil.copy2(self.trampoline, dest)
        sidecar_path = self.tmpdir / f"{name}.daemon.json"
        sidecar_path.write_text(json.dumps(sidecar), encoding="utf-8")
        return dest

    def test_trampoline_runs_command(self) -> None:
        """Trampoline reads sidecar and executes the command."""
        exe = self._setup_trampoline("test-daemon", {
            "command": sys.executable,
            "args": ["-c", "print('trampoline-ok')"],
        })
        result = subprocess.run(
            [str(exe)], capture_output=True, text=True, timeout=10,
        )
        self.assertEqual(result.returncode, 0)
        self.assertIn("trampoline-ok", result.stdout)

    def test_trampoline_sets_env(self) -> None:
        """Trampoline passes sidecar env to the child (env_clear mode)."""
        # Provide a complete env with PATH so the child can find python
        child_env = {"TEST_DAEMON_VAR": "hello123"}
        # On Windows, need SystemRoot for python to work; on Unix need PATH
        if sys.platform == "win32":
            child_env["SystemRoot"] = os.environ.get("SystemRoot", r"C:\Windows")
            child_env["PATH"] = os.environ.get("PATH", "")
        else:
            child_env["PATH"] = os.environ.get("PATH", "/usr/bin:/bin")

        exe = self._setup_trampoline("test-env-daemon", {
            "command": sys.executable,
            "args": ["-c", "import os; print(os.environ.get('TEST_DAEMON_VAR', 'MISSING'))"],
            "env": child_env,
        })
        result = subprocess.run(
            [str(exe)], capture_output=True, text=True, timeout=10,
        )
        self.assertEqual(result.returncode, 0)
        self.assertIn("hello123", result.stdout)

    def test_trampoline_sets_cwd(self) -> None:
        """Trampoline sets working directory from sidecar."""
        cwd_dir = self.tmpdir / "workdir"
        cwd_dir.mkdir()
        exe = self._setup_trampoline("test-cwd-daemon", {
            "command": sys.executable,
            "args": ["-c", "import os; print(os.getcwd())"],
            "cwd": str(cwd_dir),
        })
        result = subprocess.run(
            [str(exe)], capture_output=True, text=True, timeout=10,
        )
        self.assertEqual(result.returncode, 0)
        # Normalize paths for comparison (resolve symlinks, case)
        actual = Path(result.stdout.strip()).resolve()
        expected = cwd_dir.resolve()
        self.assertEqual(actual, expected)

    def test_trampoline_inherits_env_when_no_sidecar_env(self) -> None:
        """When sidecar has no env key, trampoline inherits parent env."""
        exe = self._setup_trampoline("test-inherit-daemon", {
            "command": sys.executable,
            "args": ["-c", "import os; print(os.environ.get('PATH', 'NO_PATH'))"],
        })
        result = subprocess.run(
            [str(exe)], capture_output=True, text=True, timeout=10,
        )
        self.assertEqual(result.returncode, 0)
        self.assertNotIn("NO_PATH", result.stdout)

    def test_trampoline_propagates_exit_code(self) -> None:
        """Trampoline exits with child's exit code."""
        exe = self._setup_trampoline("test-exit-daemon", {
            "command": sys.executable,
            "args": ["-c", "import sys; sys.exit(42)"],
        })
        result = subprocess.run(
            [str(exe)], capture_output=True, text=True, timeout=10,
        )
        self.assertEqual(result.returncode, 42)

    def test_trampoline_missing_sidecar(self) -> None:
        """Trampoline reports error when sidecar is missing."""
        ext = ".exe" if sys.platform == "win32" else ""
        dest = self.tmpdir / f"no-sidecar{ext}"
        shutil.copy2(self.trampoline, dest)
        # Do NOT write a sidecar
        result = subprocess.run(
            [str(dest)], capture_output=True, text=True, timeout=10,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("daemon-trampoline:", result.stderr)


class TestDaemonHelpers(unittest.TestCase):
    """Test the Python daemon helper functions."""

    def test_posix_process_state_parses_proc_stat(self) -> None:
        from running_process import daemon

        with (
            mock.patch.object(daemon.sys, "platform", "linux"),
            mock.patch.object(
                daemon.Path,
                "read_text",
                return_value="1234 (daemon name) Z 1 2 3 4 5\n",
            ),
        ):
            self.assertEqual(daemon._posix_process_state(1234), "Z")

    def test_daemon_handle_treats_posix_zombie_as_not_running(self) -> None:
        from running_process import daemon

        handle = daemon.DaemonHandle(
            pid=1234,
            name="test-daemon",
            runtime_dir=Path("."),
            log_path=None,
        )
        with (
            mock.patch.object(daemon.sys, "platform", "linux"),
            mock.patch.object(daemon, "_posix_child_running", return_value=None),
            mock.patch.object(daemon.os, "kill", return_value=None),
            mock.patch.object(daemon, "_posix_process_state", return_value="Z"),
        ):
            self.assertFalse(handle.is_running())

    def test_posix_ps_process_state_parses_ps_output(self) -> None:
        from running_process import daemon

        completed = subprocess.CompletedProcess(
            args=["ps"],
            returncode=0,
            stdout="Z+\n",
            stderr="",
        )
        with (
            mock.patch.object(daemon.sys, "platform", "darwin"),
            mock.patch.object(daemon.subprocess, "run", return_value=completed),
        ):
            self.assertEqual(daemon._posix_ps_process_state(1234), "Z")

    def test_daemon_handle_uses_waitpid_for_reaped_posix_child(self) -> None:
        from running_process import daemon

        handle = daemon.DaemonHandle(
            pid=1234,
            name="test-daemon",
            runtime_dir=Path("."),
            log_path=None,
        )
        with (
            mock.patch.object(daemon.sys, "platform", "darwin"),
            mock.patch.object(daemon, "_posix_child_running", return_value=False),
        ):
            self.assertFalse(handle.is_running())

    def test_daemon_handle_falls_back_to_ps_zombie_state_when_needed(self) -> None:
        from running_process import daemon

        handle = daemon.DaemonHandle(
            pid=1234,
            name="test-daemon",
            runtime_dir=Path("."),
            log_path=None,
        )
        with (
            mock.patch.object(daemon.sys, "platform", "darwin"),
            mock.patch.object(daemon, "_posix_child_running", return_value=None),
            mock.patch.object(daemon.os, "kill", return_value=None),
            mock.patch.object(daemon, "_posix_process_state", return_value=None),
            mock.patch.object(daemon, "_posix_ps_process_state", return_value="Z"),
        ):
            self.assertFalse(handle.is_running())

    def test_write_sidecar_basic(self) -> None:
        from running_process.daemon import cleanup_runtime, write_sidecar

        name = "test-sidecar-write"
        try:
            path = write_sidecar(
                name,
                command="/usr/bin/echo",
                args=["hello"],
                cwd="/tmp",
                env={"FOO": "bar"},
            )
            self.assertTrue(path.exists())
            data = json.loads(path.read_text(encoding="utf-8"))
            self.assertEqual(data["command"], "/usr/bin/echo")
            self.assertEqual(data["args"], ["hello"])
            self.assertEqual(data["cwd"], "/tmp")
            self.assertEqual(data["env"]["FOO"], "bar")
        finally:
            cleanup_runtime(name)

    def test_write_sidecar_minimal(self) -> None:
        from running_process.daemon import cleanup_runtime, write_sidecar

        name = "test-sidecar-minimal"
        try:
            path = write_sidecar(name, command="echo")
            data = json.loads(path.read_text(encoding="utf-8"))
            self.assertEqual(data["command"], "echo")
            self.assertNotIn("args", data)
            self.assertNotIn("cwd", data)
            self.assertNotIn("env", data)
        finally:
            cleanup_runtime(name)

    def test_write_sidecar_gc_metadata(self) -> None:
        from running_process.daemon import cleanup_runtime, write_sidecar

        name = "test-sidecar-gc-metadata"
        try:
            path = write_sidecar(
                name,
                command="echo",
                spawned_at_unix_ms=1234,
                last_seen_unix_ms=5678,
            )
            data = json.loads(path.read_text(encoding="utf-8"))
            self.assertEqual(data["spawned_at_unix_ms"], 1234)
            self.assertEqual(data["last_seen_unix_ms"], 5678)
        finally:
            cleanup_runtime(name)

    def test_runtime_dir_creates_path(self) -> None:
        from running_process.daemon import cleanup_runtime, runtime_dir

        name = "test-runtime-dir-create"
        try:
            d = runtime_dir(name)
            self.assertTrue(d.exists())
            self.assertTrue(d.is_dir())
            self.assertEqual(d.name, name)
        finally:
            cleanup_runtime(name)

    def test_cleanup_runtime(self) -> None:
        from running_process.daemon import cleanup_runtime, runtime_dir

        name = "test-cleanup"
        d = runtime_dir(name)
        self.assertTrue(d.exists())
        cleanup_runtime(name)
        self.assertFalse(d.exists())
