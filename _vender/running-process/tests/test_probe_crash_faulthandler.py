"""Platform-real Python crash chaining for probe S7 (#636)."""

import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


class ProbeCrashFaulthandlerTest(unittest.TestCase):
    def test_python_traceback_and_native_raw_record_both_survive_fault(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            spool = Path(directory)
            script = """
import os
from running_process.probe import ProbeConfig, install

guard = install(ProbeConfig(app_class="python-crash-fixture"), required=True)
assert guard is not None
os.abort()
"""
            env = os.environ.copy()
            env["RUNNING_PROCESS_PROBE_SPOOL_DIR"] = str(spool)
            result = subprocess.run(
                [sys.executable, "-c", script],
                check=False,
                capture_output=True,
                text=False,
                env=env,
                timeout=20,
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertIn(b"Fatal Python error: Aborted", result.stderr)
            self.assertIn(b'File "<string>"', result.stderr)
            records = list(spool.glob("*.rpcrash"))
            self.assertEqual(len(records), 1)
            raw = records[0].read_bytes()
            self.assertEqual(raw[:8], b"RPCRASH1")
            self.assertEqual(len(raw), 16 * 1024)

    def test_python_builder_can_disable_native_handler(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            spool = Path(directory)
            script = """
import os
from running_process.probe import ProbeConfig, install

guard = install(
    ProbeConfig(
        app_class="python-opt-out",
        enable_crash_handler=False,
    ),
    required=True,
)
assert guard is not None
os.abort()
"""
            env = os.environ.copy()
            env["RUNNING_PROCESS_PROBE_SPOOL_DIR"] = str(spool)
            result = subprocess.run(
                [sys.executable, "-c", script],
                check=False,
                capture_output=True,
                text=False,
                env=env,
                timeout=20,
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertIn(b"Fatal Python error: Aborted", result.stderr)
            self.assertIn(b'File "<string>"', result.stderr)
            self.assertEqual(list(spool.glob("*.rpcrash")), [])

    def test_late_faulthandler_survives_final_native_guard_close(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            spool = Path(directory)
            script = """
import faulthandler
import os
from running_process.probe import ProbeConfig, install

native_only = install(
    ProbeConfig(
        app_class="python-native-first",
        enable_faulthandler=False,
    ),
    required=True,
)
with_python = install(
    ProbeConfig(app_class="python-late-faulthandler"),
    required=True,
)
assert native_only is not None
assert with_python is not None
native_only.close()
with_python.close()
assert faulthandler.is_enabled()
os.abort()
"""
            env = os.environ.copy()
            env["RUNNING_PROCESS_PROBE_SPOOL_DIR"] = str(spool)
            result = subprocess.run(
                [sys.executable, "-c", script],
                check=False,
                capture_output=True,
                text=False,
                env=env,
                timeout=20,
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertIn(b"Fatal Python error: Aborted", result.stderr)
            self.assertIn(b'File "<string>"', result.stderr)
            self.assertEqual(
                list(spool.glob("*.rpcrash")),
                [],
                "clean guard teardown removes the unused native spool",
            )


if __name__ == "__main__":
    unittest.main()
