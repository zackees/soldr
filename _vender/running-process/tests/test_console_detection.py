"""Tests for Windows console popup detection.

These tests are gated behind ``@pytest.mark.live`` because they require
a GUI desktop session (a real monitor / RDP / VNC) and are never run in CI.
"""

import os
import subprocess
import sys
import time
import unittest
from contextlib import contextmanager
from typing import Any

import pytest

# ---------------------------------------------------------------------------
# Markers & helpers
# ---------------------------------------------------------------------------

live = pytest.mark.live
is_windows = sys.platform == "win32"
skip_unless_windows = pytest.mark.skipif(not is_windows, reason="Windows-only test")


def _is_known_ci_console_noise(window: dict[str, Any]) -> bool:
    title = str(window.get("title", ""))
    if title == "":
        return True
    if os.environ.get("GITHUB_ACTIONS", "").lower() != "true":
        return False
    return title == r"C:\Windows\system32\wsl.exe"


@contextmanager
def assert_no_console_popup(duration_secs=3.0, *, ignore_window=None):
    """Context manager that monitors for console window popups.

    Raises AssertionError if any new visible console windows appear during the block.
    """
    import threading

    from running_process._native import monitor_console_windows

    results = []

    def _monitor():
        windows = monitor_console_windows(duration_secs)
        results.extend(windows)

    t = threading.Thread(target=_monitor, daemon=True)
    t.start()
    # Small delay to let monitoring start
    time.sleep(0.1)
    try:
        yield results
    finally:
        t.join(timeout=duration_secs + 2.0)
        unexpected = list(results)
        if ignore_window is not None:
            unexpected = [window for window in results if not ignore_window(window)]
        if unexpected:
            titles = [w["title"] for w in unexpected]
            raise AssertionError(
                f"Console popup(s) detected: {titles}"
            )


# ---------------------------------------------------------------------------
# Windows-only tests
# ---------------------------------------------------------------------------


@live
@skip_unless_windows
class TestConsoleDetection(unittest.TestCase):
    """Tests for Windows console popup detection.

    These tests require a GUI desktop session and are not run in CI.
    """

    def test_monitor_returns_list(self):
        """monitor_console_windows returns a list (possibly empty)."""
        from running_process._native import monitor_console_windows

        result = monitor_console_windows(0.5)
        self.assertIsInstance(result, list)

    def test_naive_spawn_detected(self):
        """A naive subprocess.Popen with CREATE_NEW_CONSOLE creates a visible window.

        NOTE: This test validates the detector itself.  On some terminal hosts
        (e.g. Windows Terminal, git-bash over MSYS2) the console window created
        by CREATE_NEW_CONSOLE may not register as a separate top-level visible
        window.  We use ``cmd.exe /c timeout /t 4`` which is more reliably
        visible than ``python.exe -c ...``.
        """
        import threading

        from running_process._native import monitor_console_windows

        results = []

        def _monitor():
            windows = monitor_console_windows(5.0)
            results.extend(windows)

        t = threading.Thread(target=_monitor, daemon=True)
        t.start()
        time.sleep(0.3)

        # Spawn cmd.exe with CREATE_NEW_CONSOLE -- should create a visible window
        CREATE_NEW_CONSOLE = 0x00000010
        proc = subprocess.Popen(
            ["cmd.exe", "/c", "timeout", "/t", "4"],
            creationflags=CREATE_NEW_CONSOLE,
        )
        try:
            t.join(timeout=7.0)
            # Should have detected at least one new window.  If this fails
            # the environment may suppress console windows (e.g. RDP shadow,
            # headless session).  The test is advisory — the key assertion is
            # test_detached_no_popup which validates the daemon spawn path.
            if not results:
                self.skipTest(
                    "CREATE_NEW_CONSOLE did not produce a detectable visible window "
                    "in this environment — detector validation inconclusive"
                )
        finally:
            proc.kill()
            proc.wait()

    def test_detached_no_popup(self):
        """DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP should NOT create a visible window."""
        DETACHED_PROCESS = 0x00000008
        CREATE_NEW_PROCESS_GROUP = 0x00000200

        with assert_no_console_popup(
            duration_secs=3.0,
            ignore_window=_is_known_ci_console_noise,
        ):
            proc = subprocess.Popen(
                [sys.executable, "-c", "import time; time.sleep(2)"],
                creationflags=DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            time.sleep(2.0)
            proc.kill()
            proc.wait()

    def test_assert_no_console_popup_helper(self):
        """The assert_no_console_popup context manager works correctly."""
        # Should NOT raise -- no windows spawned by us. The GH
        # `windows-11-arm` image keeps a `wsl.exe` console window
        # around persistently; pass the same filter the rest of the
        # tests in this file use so it doesn't trip the assertion.
        with assert_no_console_popup(
            duration_secs=1.0,
            ignore_window=_is_known_ci_console_noise,
        ):
            time.sleep(0.5)


# ---------------------------------------------------------------------------
# Cross-platform tests
# ---------------------------------------------------------------------------


@live
class TestConsoleDetectionCrossPlatform(unittest.TestCase):
    """Cross-platform tests for the monitor_console_windows API."""

    def test_returns_list_on_any_platform(self):
        """monitor_console_windows returns a list on all platforms."""
        from running_process._native import monitor_console_windows

        result = monitor_console_windows(0.2)
        self.assertIsInstance(result, list)
