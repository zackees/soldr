#!/usr/bin/env python3
"""Tests for verified catalogue downloads."""

from __future__ import annotations

import io
import tempfile
import unittest
from pathlib import Path
from typing import Any
from unittest import mock

from _script_loader import load_sibling_script

script = load_sibling_script("download_catalogued_asset")


class DownloadCataloguedAssetTests(unittest.TestCase):
    def test_valid_download_is_written_and_reported(self) -> None:
        payload = b"catalogue payload"
        metadata: dict[str, Any] = {
            "filename": "asset.tar.gz",
            "urls": ["https://example.test/asset.tar.gz"],
            "sha256": script.hashlib.sha256(payload).hexdigest(),
        }
        response = mock.MagicMock()
        response.__enter__.return_value = io.BytesIO(payload)
        with (
            tempfile.TemporaryDirectory() as temp,
            mock.patch.object(script.urllib.request, "urlopen", return_value=response),
        ):
            output = Path(temp) / metadata["filename"]
            result = script.download_verified(metadata, output)
            self.assertEqual(output.read_bytes(), payload)
            self.assertEqual(result["verified_sha256"], metadata["sha256"])

    def test_mismatch_fails_before_publish(self) -> None:
        metadata: dict[str, Any] = {
            "filename": "asset.tar.gz",
            "urls": ["https://example.test/asset.tar.gz"],
            "sha256": "0" * 64,
        }
        response = mock.MagicMock()
        response.__enter__.return_value = io.BytesIO(b"tampered")
        with (
            tempfile.TemporaryDirectory() as temp,
            mock.patch.object(script.urllib.request, "urlopen", return_value=response),
        ):
            with self.assertRaisesRegex(SystemExit, "sha256 mismatch"):
                script.download_verified(metadata, Path(temp) / metadata["filename"])
            self.assertFalse((Path(temp) / metadata["filename"]).exists())

    def test_missing_digest_fails_closed(self) -> None:
        metadata: dict[str, Any] = {
            "filename": "asset.tar.gz",
            "urls": ["https://example.test"],
        }
        with tempfile.TemporaryDirectory() as temp:
            with self.assertRaisesRegex(SystemExit, "no valid sha256"):
                script.download_verified(metadata, Path(temp) / metadata["filename"])


if __name__ == "__main__":
    unittest.main()
