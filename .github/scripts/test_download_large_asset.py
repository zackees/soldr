#!/usr/bin/env python3
"""Behavior tests for the stall-aware large artifact curl policy."""

from __future__ import annotations
import hashlib
import os
import subprocess
import tempfile
import textwrap
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent
HELPER = ROOT / "download_large_asset.sh"


class DownloadLargeAssetTests(unittest.TestCase):
    def run_helper(self, mock_body: str, payload: bytes, *, size: int | None = None):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            mock = root / "curl"
            output = root / "asset"
            mock.write_text(
                "#!/usr/bin/env bash\nset -euo pipefail\n" + textwrap.dedent(mock_body)
            )
            mock.chmod(0o755)
            env = os.environ | {
                "SOLDR_DOWNLOAD_CURL_BIN": str(mock),
                "SOLDR_DOWNLOAD_RETRIES": "2",
            }
            command = [
                "bash",
                str(HELPER),
                "--url",
                "https://example.test/a",
                "--output",
                str(output),
                "--sha256",
                hashlib.sha256(payload).hexdigest(),
            ]
            if size is not None:
                command += ["--expected-size", str(size)]
            result = subprocess.run(command, capture_output=True, text=True, env=env)
            return (
                result,
                output.exists(),
                output.read_bytes() if output.exists() else b"",
            )

    def test_steady_slow_progress_succeeds(self):
        result, exists, body = self.run_helper(
            'while [ "$1" != --output ]; do shift; done; printf slow > "$2"\n',
            b"slow",
            size=4,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(exists)
        self.assertEqual(body, b"slow")

    def test_stall_is_reported(self):
        result, _, _ = self.run_helper("exit 28\n", b"anything")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("failure=stall", result.stderr)

    def test_truncation_retries_with_partial_file(self):
        result, exists, body = self.run_helper(
            'while [ "$1" != --output ]; do shift; done; out="$2"; if [ ! -f "$out" ]; then printf ab > "$out"; else printf cd >> "$out"; fi\n',
            b"abcd",
            size=4,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(exists)
        self.assertEqual(body, b"abcd")

    def test_integrity_mismatch_is_hard_failure(self):
        result, exists, _ = self.run_helper(
            'while [ "$1" != --output ]; do shift; done; printf tampered > "$2"\n',
            b"expected",
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("failure=integrity", result.stderr)
        self.assertFalse(exists)


if __name__ == "__main__":
    unittest.main()
