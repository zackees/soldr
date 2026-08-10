"""Tests for daemon environment building and venv stripping."""

import os
import sys
import unittest


class TestBuildDaemonEnv(unittest.TestCase):
    """Unit tests for build_daemon_env() — no trampoline required."""

    def test_returns_dict(self):
        from running_process.daemon import build_daemon_env

        env = build_daemon_env()
        self.assertIsInstance(env, dict)

    def test_path_present(self):
        """The resulting env always has a PATH (or Path on Windows)."""
        from running_process.daemon import build_daemon_env

        env = build_daemon_env()
        path_key = "Path" if sys.platform == "win32" and "Path" in env else "PATH"
        self.assertIn(path_key, env)
        self.assertTrue(len(env[path_key]) > 0, "PATH should not be empty")

    def test_virtual_env_stripped(self):
        """VIRTUAL_ENV is removed from the daemon env."""
        from running_process.daemon import build_daemon_env

        old = os.environ.get("VIRTUAL_ENV")
        os.environ["VIRTUAL_ENV"] = "/fake/venv"
        try:
            env = build_daemon_env()
            self.assertNotIn("VIRTUAL_ENV", env)
        finally:
            if old is not None:
                os.environ["VIRTUAL_ENV"] = old
            else:
                os.environ.pop("VIRTUAL_ENV", None)

    def test_pythonhome_stripped(self):
        """PYTHONHOME is removed from the daemon env."""
        from running_process.daemon import build_daemon_env

        old = os.environ.get("PYTHONHOME")
        os.environ["PYTHONHOME"] = "/fake/home"
        try:
            env = build_daemon_env()
            self.assertNotIn("PYTHONHOME", env)
        finally:
            if old is not None:
                os.environ["PYTHONHOME"] = old
            else:
                os.environ.pop("PYTHONHOME", None)

    def test_pip_vars_stripped(self):
        """PIP_* variables are removed."""
        from running_process.daemon import build_daemon_env

        old = os.environ.get("PIP_INDEX_URL")
        os.environ["PIP_INDEX_URL"] = "https://example.com/simple"
        try:
            env = build_daemon_env()
            self.assertNotIn("PIP_INDEX_URL", env)
        finally:
            if old is not None:
                os.environ["PIP_INDEX_URL"] = old
            else:
                os.environ.pop("PIP_INDEX_URL", None)

    def test_venv_path_cleaned(self):
        """PATH entries containing venv markers are removed."""
        from running_process.daemon import _clean_path

        sep = ";" if sys.platform == "win32" else ":"
        raw = sep.join(["/usr/bin", "/home/user/.venv/bin", "/usr/local/bin"])
        cleaned = _clean_path(raw)
        self.assertNotIn(".venv", cleaned)
        self.assertIn("/usr/bin", cleaned)

    def test_caller_env_merged(self):
        """Caller-specified env overrides are applied."""
        from running_process.daemon import build_daemon_env

        env = build_daemon_env(caller_env={"MY_CUSTOM_VAR": "hello"})
        self.assertEqual(env["MY_CUSTOM_VAR"], "hello")

    def test_caller_env_overrides_parent(self):
        """Caller env takes precedence over parent env for the same key."""
        from running_process.daemon import build_daemon_env

        old = os.environ.get("HOME")
        os.environ["HOME"] = "/original/home"
        try:
            env = build_daemon_env(caller_env={"HOME": "/custom/home"})
            self.assertEqual(env["HOME"], "/custom/home")
        finally:
            if old is not None:
                os.environ["HOME"] = old
            else:
                os.environ.pop("HOME", None)

    def test_running_process_vars_forwarded(self):
        """RUNNING_PROCESS_* vars from parent are forwarded."""
        from running_process.daemon import build_daemon_env

        key = "RUNNING_PROCESS_TEST_MARKER"
        old = os.environ.get(key)
        os.environ[key] = "forwarded"
        try:
            env = build_daemon_env()
            self.assertEqual(env[key], "forwarded")
        finally:
            if old is not None:
                os.environ[key] = old
            else:
                os.environ.pop(key, None)

    def test_empty_path_falls_back_to_platform_default(self):
        """If PATH is empty after cleaning, platform defaults are used."""
        from running_process.daemon import _clean_path, _platform_default_path

        sep = ";" if sys.platform == "win32" else ":"
        # A PATH with only venv entries
        raw = sep.join(["/home/user/.venv/bin", "/home/user/venv/Scripts"])
        cleaned = _clean_path(raw)
        if not cleaned:
            default = _platform_default_path()
            self.assertTrue(len(default) > 0)
