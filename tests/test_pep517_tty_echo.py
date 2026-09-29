"""The PEP 517 relay must not echo terminal input reports over the build."""

# pylint: disable=possibly-used-before-assignment
from __future__ import annotations

import os
import sys
import unittest
from pathlib import Path
from unittest import mock

from conftest import load_script_module

quiet_tty_echo = load_script_module(
    Path(__file__).resolve().parents[1] / "src" / "soldr" / "_tty_quiet.py"
).quiet_tty_echo

if sys.platform != "win32":
    import pty
    import termios


@unittest.skipIf(sys.platform == "win32", "POSIX tty semantics")
class QuietTtyEchoTest(unittest.TestCase):
    def setUp(self) -> None:
        self.master, self.slave = pty.openpty()
        self.addCleanup(os.close, self.master)
        self.addCleanup(os.close, self.slave)
        stdin = open(  # pylint: disable=consider-using-with
            self.slave, closefd=False, encoding="utf-8"
        )
        self.addCleanup(stdin.close)
        patcher = mock.patch.object(sys, "stdin", stdin)
        patcher.start()
        self.addCleanup(patcher.stop)
        env = mock.patch.dict(os.environ)
        env.start()
        self.addCleanup(env.stop)
        os.environ.pop("SOLDR_PEP517_TTY_ECHO", None)

    def lflag(self) -> int:
        return int(termios.tcgetattr(self.slave)[3])

    def test_echo_off_inside_then_restored_and_input_flushed(self) -> None:
        before = termios.tcgetattr(self.slave)
        self.assertTrue(before[3] & termios.ECHO)
        with quiet_tty_echo():
            self.assertFalse(self.lflag() & termios.ECHO)
            self.assertTrue(self.lflag() & termios.ISIG)
            os.write(self.master, b"\x1b[I\x1b[B\n")
        self.assertEqual(termios.tcgetattr(self.slave), before)
        os.set_blocking(self.slave, False)
        with self.assertRaises(BlockingIOError):
            os.read(self.slave, 64)

    def test_opt_out_keeps_echo(self) -> None:
        os.environ["SOLDR_PEP517_TTY_ECHO"] = "1"
        with quiet_tty_echo():
            self.assertTrue(self.lflag() & termios.ECHO)

    def test_restores_when_block_raises(self) -> None:
        before = termios.tcgetattr(self.slave)
        with self.assertRaises(RuntimeError):
            with quiet_tty_echo():
                raise RuntimeError("build failed")
        self.assertEqual(termios.tcgetattr(self.slave), before)

    def test_non_tty_stdin_is_noop(self) -> None:
        read_fd, write_fd = os.pipe()
        self.addCleanup(os.close, read_fd)
        self.addCleanup(os.close, write_fd)
        with open(read_fd, closefd=False, encoding="utf-8") as pipe_in:
            with mock.patch.object(sys, "stdin", pipe_in):
                with quiet_tty_echo():
                    pass


if __name__ == "__main__":
    unittest.main()
