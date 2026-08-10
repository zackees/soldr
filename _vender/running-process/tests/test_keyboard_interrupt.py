"""Tests for KeyboardInterrupt propagation across subprocess and PTY modes.

These tests verify that:
- SIGINT exit codes are correctly detected and raise KeyboardInterrupt
- SIGSEGV and other non-KBI exit codes do NOT raise KeyboardInterrupt
- The interrupt_handler utility works for cross-thread notification
- send_interrupt() works in subprocess mode
"""

from __future__ import annotations

import subprocess
import sys
import threading
import unittest

from running_process import RunningProcess
from running_process.exit_status import classify_exit_status
from running_process.interrupt_handler import handle_keyboard_interrupt, is_main_thread


class TestKeyboardInterruptExitCodes(unittest.TestCase):
    """Verify exit code classification for KBI vs non-KBI signals."""

    KBI_CODES = RunningProcess.KEYBOARD_INTERRUPT_EXIT_CODES

    def test_sigint_negative_is_kbi(self) -> None:
        """Unix SIGINT as negative exit code (-2) should be KBI."""
        self.assertIn(-2, self.KBI_CODES)

    def test_sigint_128_plus_2_is_kbi(self) -> None:
        """Unix 128+SIGINT (130) should be KBI."""
        self.assertIn(130, self.KBI_CODES)

    def test_windows_status_control_c_exit_signed_is_kbi(self) -> None:
        """Windows STATUS_CONTROL_C_EXIT (signed) should be KBI."""
        self.assertIn(-1073741510, self.KBI_CODES)

    def test_windows_status_control_c_exit_unsigned_is_kbi(self) -> None:
        """Windows STATUS_CONTROL_C_EXIT (unsigned) should be KBI."""
        self.assertIn(3221225786, self.KBI_CODES)

    def test_sigsegv_is_not_kbi(self) -> None:
        """SIGSEGV (-11) must NOT be treated as KBI."""
        self.assertNotIn(-11, self.KBI_CODES)

    def test_generic_255_is_not_kbi(self) -> None:
        """Generic error 255 must NOT be treated as KBI."""
        self.assertNotIn(255, self.KBI_CODES)

    def test_zero_is_not_kbi(self) -> None:
        """Success (0) is not KBI."""
        self.assertNotIn(0, self.KBI_CODES)

    def test_one_is_not_kbi(self) -> None:
        """Generic failure (1) is not KBI."""
        self.assertNotIn(1, self.KBI_CODES)


class TestExitStatusClassification(unittest.TestCase):
    """Verify classify_exit_status correctly identifies interrupted exits."""

    KBI_CODES = RunningProcess.KEYBOARD_INTERRUPT_EXIT_CODES

    def test_sigint_classified_as_interrupted(self) -> None:
        status = classify_exit_status(-2, self.KBI_CODES)
        self.assertTrue(status.interrupted)
        self.assertFalse(status.abnormal)

    def test_normal_exit_not_interrupted(self) -> None:
        status = classify_exit_status(0, self.KBI_CODES)
        self.assertFalse(status.interrupted)
        self.assertFalse(status.abnormal)

    def test_abnormal_exit_not_interrupted(self) -> None:
        status = classify_exit_status(1, self.KBI_CODES)
        self.assertFalse(status.interrupted)
        self.assertTrue(status.abnormal)

    def test_sigsegv_is_abnormal_not_interrupted(self) -> None:
        """SIGSEGV should be abnormal, NOT interrupted."""
        status = classify_exit_status(-11, self.KBI_CODES)
        self.assertFalse(status.interrupted)
        self.assertTrue(status.abnormal)


class TestInterruptHandler(unittest.TestCase):
    """Verify the cross-thread interrupt handler utility."""

    def test_is_main_thread_true_in_main(self) -> None:
        self.assertTrue(is_main_thread())

    def test_is_main_thread_false_in_worker(self) -> None:
        result = [None]

        def worker() -> None:
            result[0] = is_main_thread()

        t = threading.Thread(target=worker)
        t.start()
        t.join()
        self.assertFalse(result[0])

    def test_handle_keyboard_interrupt_raises_in_main(self) -> None:
        with self.assertRaises(KeyboardInterrupt):
            handle_keyboard_interrupt(KeyboardInterrupt())


class TestSubprocessInterrupt(unittest.TestCase):
    """Verify send_interrupt() and KBI propagation in subprocess mode."""

    def test_send_interrupt_causes_kbi_exit_code(self) -> None:
        """Sending interrupt to a child should cause a KBI exit code."""
        creationflags = (
            getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0)
            if sys.platform == "win32"
            else None
        )
        process = RunningProcess(
            [
                sys.executable,
                "-c",
                (
                    "import signal, sys, time\n"
                    "print('ready', flush=True)\n"
                    "def h(s, f): sys.exit(130)\n"
                    "signal.signal(signal.SIGINT, h)\n"
                    "time.sleep(2)\n"
                ),
            ],
            creationflags=creationflags,
            timeout=10,
        )
        line = process.get_next_stdout_line(timeout=5)
        self.assertEqual(line, "ready")
        process.send_interrupt()
        with self.assertRaises(KeyboardInterrupt):
            process.wait(timeout=5)

    def test_normal_exit_does_not_raise_kbi(self) -> None:
        """A process exiting normally should NOT raise KeyboardInterrupt."""
        process = RunningProcess(
            [sys.executable, "-c", "print('done', flush=True)"],
            timeout=5,
        )
        code = process.wait()
        self.assertEqual(code, 0)

    def test_abnormal_exit_does_not_raise_kbi(self) -> None:
        """A process exiting with code 1 should NOT raise KeyboardInterrupt."""
        process = RunningProcess(
            [sys.executable, "-c", "import sys; sys.exit(1)"],
            timeout=5,
        )
        code = process.wait()
        self.assertEqual(code, 1)


if __name__ == "__main__":
    unittest.main()
