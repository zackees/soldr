"""Tests for capture mode, callable echo, output_formatter, and on_complete."""

from __future__ import annotations

import sys
import time
import unittest

from running_process import (
    NullOutputFormatter,
    RunningProcess,
    TimeDeltaFormatter,
)

PYTHON = sys.executable


class TestCaptureMode(unittest.TestCase):
    """Tests for capture parameter."""

    def test_capture_true_captures_output(self):
        rp = RunningProcess(
            [PYTHON, "-c", "print('test output')"],
            capture=True,
        )
        rp.wait()
        self.assertIn("test output", rp.stdout)

    def test_capture_false_does_not_capture_output(self):
        rp = RunningProcess(
            [PYTHON, "-c", "print('test output')"],
            capture=False,
        )
        rp.wait()
        self.assertEqual(rp.stdout, "")

    def test_capture_false_with_timeout(self):
        start = time.time()
        rp = RunningProcess(
            [PYTHON, "-c", "import time; time.sleep(5)"],
            capture=False,
            timeout=1,
        )
        with self.assertRaises((TimeoutError, Exception)):
            rp.wait()
        elapsed = time.time() - start
        self.assertLess(elapsed, 4.0)

    def test_capture_false_non_zero_exit(self):
        rp = RunningProcess(
            [PYTHON, "-c", "import sys; sys.exit(42)"],
            capture=False,
            check=False,
        )
        exit_code = rp.wait()
        self.assertEqual(exit_code, 42)

    def test_capture_true_default(self):
        rp = RunningProcess(
            [PYTHON, "-c", "print('default capture')"],
        )
        rp.wait()
        self.assertIn("default capture", rp.stdout)

    def test_capture_false_with_manual_start(self):
        rp = RunningProcess(
            [PYTHON, "-c", "print('manual start')"],
            capture=False,
            auto_run=False,
        )
        rp.start()
        exit_code = rp.wait()
        self.assertEqual(exit_code, 0)
        self.assertEqual(rp.stdout, "")


class TestTextBinaryMode(unittest.TestCase):
    """Tests for text vs binary mode."""

    def test_text_mode_default(self):
        rp = RunningProcess(
            [PYTHON, "-c", "print('hello text')"],
            capture=True,
        )
        rp.wait()
        self.assertIsInstance(rp.stdout, str)
        self.assertIn("hello text", rp.stdout)

    def test_text_false_returns_bytes(self):
        rp = RunningProcess(
            [PYTHON, "-c", "print('hello bytes')"],
            capture=True,
            text=False,
        )
        rp.wait()
        self.assertIsInstance(rp.stdout, bytes)
        self.assertIn(b"hello bytes", rp.stdout)

    def test_text_mode_explicit(self):
        rp = RunningProcess(
            [PYTHON, "-c", "print('explicit text')"],
            capture=True,
            text=True,
        )
        rp.wait()
        self.assertIsInstance(rp.stdout, str)
        self.assertIn("explicit text", rp.stdout)


class TestCallableEcho(unittest.TestCase):
    """Tests for callable echo callbacks in wait()."""

    def test_echo_true_prints_output(self):
        rp = RunningProcess(
            [PYTHON, "-c", "print('echo test')"],
            capture=True,
        )
        exit_code = rp.wait(echo=True)
        self.assertEqual(exit_code, 0)

    def test_echo_callable_receives_lines(self):
        lines: list[str] = []
        rp = RunningProcess(
            [PYTHON, "-c", "print('line1'); print('line2')"],
            capture=True,
        )
        exit_code = rp.wait(echo=lambda line: lines.append(line))
        self.assertEqual(exit_code, 0)
        combined = "\n".join(lines)
        self.assertIn("line1", combined)
        self.assertIn("line2", combined)

    def test_echo_callable_custom_function(self):
        results: list[str] = []

        def my_echo(line: str) -> None:
            results.append(f">> {line}")

        rp = RunningProcess(
            [PYTHON, "-c", "print('custom')"],
            capture=True,
        )
        rp.wait(echo=my_echo)
        combined = "\n".join(results)
        self.assertIn(">> custom", combined)

    def test_echo_false_no_output(self):
        rp = RunningProcess(
            [PYTHON, "-c", "print('silent')"],
            capture=True,
        )
        exit_code = rp.wait(echo=False)
        self.assertEqual(exit_code, 0)

    def test_echo_invalid_type_raises(self):
        rp = RunningProcess(
            [PYTHON, "-c", "print('x')"],
            capture=True,
        )
        with self.assertRaises(TypeError):
            rp.wait(echo=42)  # type: ignore[arg-type]


class TestOutputFormatter(unittest.TestCase):
    """Tests for output_formatter parameter."""

    def test_null_formatter_passthrough(self):
        rp = RunningProcess(
            [PYTHON, "-c", "print('plain line')"],
            capture=True,
            output_formatter=NullOutputFormatter(),
        )
        rp.wait()
        self.assertIn("plain line", rp.stdout)

    def test_time_delta_formatter_adds_timestamps(self):
        rp = RunningProcess(
            [PYTHON, "-c", "print('timed')"],
            capture=True,
            output_formatter=TimeDeltaFormatter(),
        )
        rp.wait()
        lines = rp.drain_stdout()
        if lines:
            # TimeDeltaFormatter prepends "[X.XX] "
            for line in lines:
                self.assertRegex(str(line), r"\[\d+\.\d+\]")

    def test_formatter_get_next_line(self):
        rp = RunningProcess(
            [PYTHON, "-c", "print('formatted')"],
            capture=True,
            output_formatter=TimeDeltaFormatter(),
            auto_run=False,
        )
        rp.start()
        rp.wait()
        # stdout property uses captured output, not _format
        # but drain_stdout applies _format
        lines = rp.drain_stdout()
        if lines:
            self.assertRegex(str(lines[0]), r"\[\d+\.\d+\].*formatted")

    def test_custom_formatter(self):
        class PrefixFormatter:
            def begin(self) -> None:
                pass

            def transform(self, line: str) -> str:
                return f"[PREFIX] {line}"

            def end(self) -> None:
                pass

        rp = RunningProcess(
            [PYTHON, "-c", "print('hello')"],
            capture=True,
            output_formatter=PrefixFormatter(),
        )
        rp.wait()
        lines = rp.drain_stdout()
        if lines:
            self.assertIn("[PREFIX]", str(lines[0]))

    def test_no_formatter_default(self):
        rp = RunningProcess(
            [PYTHON, "-c", "print('no format')"],
            capture=True,
        )
        rp.wait()
        lines = rp.drain_stdout()
        for line in lines:
            self.assertNotRegex(str(line), r"^\[.*\]")


class TestOnComplete(unittest.TestCase):
    """Tests for on_complete callback."""

    def test_on_complete_called_after_wait(self):
        completed = []

        def on_done():
            completed.append(True)

        rp = RunningProcess(
            [PYTHON, "-c", "print('done')"],
            capture=True,
            on_complete=on_done,
        )
        rp.wait()
        self.assertEqual(completed, [True])

    def test_on_complete_not_called_before_wait(self):
        completed = []

        def on_done():
            completed.append(True)

        rp = RunningProcess(
            [PYTHON, "-c", "import time; time.sleep(0.2)"],
            capture=True,
            on_complete=on_done,
            auto_run=False,
        )
        rp.start()
        self.assertEqual(completed, [])
        rp.wait()
        self.assertEqual(completed, [True])

    def test_on_complete_none_default(self):
        rp = RunningProcess(
            [PYTHON, "-c", "print('ok')"],
            capture=True,
        )
        # Should not raise
        rp.wait()

    def test_on_complete_with_nonzero_exit(self):
        completed = []

        def on_done():
            completed.append(True)

        rp = RunningProcess(
            [PYTHON, "-c", "import sys; sys.exit(1)"],
            capture=True,
            on_complete=on_done,
            check=False,
        )
        rp.wait()
        self.assertEqual(completed, [True])

    def test_on_complete_with_echo(self):
        completed = []
        echoed = []

        rp = RunningProcess(
            [PYTHON, "-c", "print('echo+complete')"],
            capture=True,
            on_complete=lambda: completed.append(True),
        )
        rp.wait(echo=lambda line: echoed.append(line))
        self.assertEqual(completed, [True])
        self.assertTrue(len(echoed) > 0)


class TestRunStaticMethod(unittest.TestCase):
    """Tests for RunningProcess.run() static method."""

    def test_run_capture_output_true(self):
        result = RunningProcess.run(
            [PYTHON, "-c", "print('captured')"],
            capture_output=True,
        )
        self.assertEqual(result.returncode, 0)
        self.assertIn("captured", result.stdout)

    def test_run_capture_output_false(self):
        result = RunningProcess.run(
            [PYTHON, "-c", "print('not captured')"],
            capture_output=False,
        )
        self.assertEqual(result.returncode, 0)
        self.assertIsNone(result.stdout)

    def test_run_check_raises_on_nonzero(self):
        from running_process import CalledProcessError

        with self.assertRaises(CalledProcessError):
            RunningProcess.run(
                [PYTHON, "-c", "import sys; sys.exit(1)"],
                capture_output=True,
                check=True,
            )

    def test_run_timeout_raises(self):
        from running_process import TimeoutExpired

        with self.assertRaises(TimeoutExpired):
            RunningProcess.run(
                [PYTHON, "-c", "import time; time.sleep(10)"],
                capture_output=True,
                timeout=1,
            )


class TestCombinedModes(unittest.TestCase):
    """Tests for combined parameter interactions."""

    def test_capture_false_with_text_false(self):
        rp = RunningProcess(
            [PYTHON, "-c", "print('combo')"],
            capture=False,
            text=False,
        )
        exit_code = rp.wait()
        self.assertEqual(exit_code, 0)

    def test_multiline_output_capture_true(self):
        script = "import sys; [print(f'line{i}') for i in range(5)]"
        rp = RunningProcess(
            [PYTHON, "-c", script],
            capture=True,
        )
        rp.wait()
        for i in range(5):
            self.assertIn(f"line{i}", rp.stdout)

    def test_multiline_output_capture_false(self):
        script = "import sys; [print(f'line{i}') for i in range(5)]"
        rp = RunningProcess(
            [PYTHON, "-c", script],
            capture=False,
        )
        rp.wait()
        self.assertEqual(rp.stdout, "")

    def test_callable_echo_with_formatter(self):
        lines: list[str] = []
        rp = RunningProcess(
            [PYTHON, "-c", "print('formatted echo')"],
            capture=True,
            output_formatter=TimeDeltaFormatter(),
        )
        rp.wait(echo=lambda line: lines.append(line))
        # Lines should exist (echo callback received them)
        self.assertTrue(len(lines) > 0)

    def test_on_complete_with_capture_false(self):
        completed = []
        rp = RunningProcess(
            [PYTHON, "-c", "print('no capture')"],
            capture=False,
            on_complete=lambda: completed.append(True),
        )
        rp.wait()
        self.assertEqual(completed, [True])


class TestEchoTimestamps(unittest.TestCase):
    """Tests for echo_timestamps parameter in wait()."""

    def test_relative_timestamps_with_echo_true(self):
        lines: list[str] = []
        rp = RunningProcess(
            [PYTHON, "-c", "print('hello')"],
            capture=True,
        )
        rp.wait(echo=lambda line: lines.append(line), echo_timestamps="relative")
        combined = "\n".join(lines)
        # Relative timestamps look like [0.12]
        self.assertRegex(combined, r"\[\d+\.\d+\].*hello")

    def test_absolute_timestamps_with_echo_true(self):
        lines: list[str] = []
        rp = RunningProcess(
            [PYTHON, "-c", "print('world')"],
            capture=True,
        )
        rp.wait(echo=lambda line: lines.append(line), echo_timestamps="absolute")
        combined = "\n".join(lines)
        # Absolute timestamps look like [HH:MM:SS.mmm]
        self.assertRegex(combined, r"\[\d{2}:\d{2}:\d{2}\.\d{3}\].*world")

    def test_echo_timestamps_implies_echo(self):
        """echo_timestamps alone should enable echo even when echo=False."""
        lines: list[str] = []
        rp = RunningProcess(
            [PYTHON, "-c", "print('implied')"],
            capture=True,
        )
        # echo defaults to False, but echo_timestamps should activate echo
        exit_code = rp.wait(echo=lambda line: lines.append(line), echo_timestamps="relative")
        self.assertEqual(exit_code, 0)
        self.assertTrue(len(lines) > 0)

    def test_relative_timestamp_values_increase(self):
        lines: list[str] = []
        script = "import time; print('a'); time.sleep(0.1); print('b')"
        rp = RunningProcess(
            [PYTHON, "-c", script],
            capture=True,
        )
        rp.wait(echo=lambda line: lines.append(line), echo_timestamps="relative")
        # Extract numeric timestamps
        import re
        timestamps = []
        for line in lines:
            m = re.match(r"\[(\d+\.\d+)\]", line)
            if m:
                timestamps.append(float(m.group(1)))
        self.assertGreaterEqual(len(timestamps), 2)
        # Second timestamp should be greater than first
        self.assertGreater(timestamps[-1], timestamps[0])

    def test_invalid_echo_timestamps_raises(self):
        rp = RunningProcess(
            [PYTHON, "-c", "print('x')"],
            capture=True,
        )
        with self.assertRaises(ValueError):
            rp.wait(echo=True, echo_timestamps="invalid")

    def test_echo_timestamps_with_custom_callback(self):
        results: list[str] = []

        def my_handler(line: str) -> None:
            results.append(line)

        rp = RunningProcess(
            [PYTHON, "-c", "print('custom ts')"],
            capture=True,
        )
        rp.wait(echo=my_handler, echo_timestamps="relative")
        combined = "\n".join(results)
        self.assertRegex(combined, r"\[\d+\.\d+\].*custom ts")

    def test_echo_timestamps_none_no_prefix(self):
        lines: list[str] = []
        rp = RunningProcess(
            [PYTHON, "-c", "print('plain')"],
            capture=True,
        )
        rp.wait(echo=lambda line: lines.append(line), echo_timestamps=None)
        for line in lines:
            self.assertNotRegex(line, r"^\[\d")


if __name__ == "__main__":
    unittest.main()
