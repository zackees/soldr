#!/usr/bin/env python3
"""Tests for the hash-verified catalogue tool installer."""

from __future__ import annotations

import hashlib
import io
import tarfile
import tempfile
import unittest
import urllib.error
from pathlib import Path
from unittest import mock

from _script_loader import load_sibling_script

script = load_sibling_script("install_catalogued_tools")

TARGETS = [
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
]


class InstallCataloguedToolsTests(unittest.TestCase):
    def test_catalogue_fetch_retries_a_transient_failure(self) -> None:
        response = io.BytesIO(b'{"schema_version": 1, "entries": []}')
        with (
            mock.patch.object(
                script.urllib.request,
                "urlopen",
                side_effect=[urllib.error.URLError("reset"), response],
            ) as urlopen,
            mock.patch.object(script.time, "sleep") as sleep,
        ):
            catalogue = script.fetch_catalogue("https://example.test/catalogue.json")

        self.assertEqual(catalogue["schema_version"], 1)
        self.assertEqual(urlopen.call_count, 2)
        sleep.assert_called_once_with(script.RETRY_BASE_DELAY_SECS)

    def test_asset_download_retries_then_verifies_sha256(self) -> None:
        payload = b"catalogued binary"
        entry = {
            "url": "https://example.test/tool.tar.gz",
            "sha256": hashlib.sha256(payload).hexdigest(),
        }
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp) / "tool.tar.gz"
            with (
                mock.patch.object(
                    script.urllib.request,
                    "urlopen",
                    side_effect=[
                        urllib.error.URLError("reset"),
                        io.BytesIO(payload),
                    ],
                ) as urlopen,
                mock.patch.object(script.time, "sleep"),
            ):
                script.download_verified(entry, output)

            self.assertEqual(urlopen.call_count, 2)
            self.assertEqual(output.read_bytes(), payload)

    def test_all_supported_targets_have_exact_asset_names(self) -> None:
        names = {
            script.asset_name("cargo-dylint", "6.0.3", target) for target in TARGETS
        }
        self.assertEqual(len(names), 8)
        self.assertEqual(
            script.asset_name("cargo-dylint", "6.0.3", "aarch64-pc-windows-msvc"),
            "cargo-dylint-6.0.3-aarch64-pc-windows-msvc.tar.gz",
        )

    def test_select_entry_requires_one_exact_hash_pinned_row(self) -> None:
        expected = script.asset_name("dylint-link", "6.0.3", "x86_64-unknown-linux-gnu")
        row = {
            "asset": expected,
            "url": "https://example.test/dylint-link.tar.gz",
            "sha256": "a" * 64,
        }
        self.assertEqual(
            script.select_entry(
                {"schema_version": 1, "entries": [row]},
                tool="dylint-link",
                version="6.0.3",
                target="x86_64-unknown-linux-gnu",
            ),
            row,
        )
        with self.assertRaisesRegex(SystemExit, "exactly one"):
            script.select_entry(
                {"schema_version": 1, "entries": []},
                tool="dylint-link",
                version="6.0.3",
                target="x86_64-unknown-linux-gnu",
            )
        with self.assertRaisesRegex(SystemExit, "valid sha256"):
            script.select_entry(
                {"schema_version": 1, "entries": [{**row, "sha256": "bad"}]},
                tool="dylint-link",
                version="6.0.3",
                target="x86_64-unknown-linux-gnu",
            )

    def test_extract_binary_rejects_tar_symlinks(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            archive = root / "asset.tar.gz"
            with tarfile.open(archive, "w:gz") as handle:
                link = tarfile.TarInfo("cargo-dylint")
                link.type = tarfile.SYMTYPE
                link.linkname = "outside"
                handle.addfile(link)
            with self.assertRaisesRegex(SystemExit, "unsafe path"):
                script.extract_binary(
                    archive,
                    tool="cargo-dylint",
                    target="x86_64-unknown-linux-gnu",
                    output_dir=root / "bin",
                )

    def test_extract_binary_materializes_only_the_expected_executable(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            archive = root / "asset.tar.gz"
            payload = b"#!/bin/sh\necho cargo-dylint 6.0.3\n"
            with tarfile.open(archive, "w:gz") as handle:
                member = tarfile.TarInfo("nested/cargo-dylint")
                member.mode = 0o755
                member.size = len(payload)
                handle.addfile(member, io.BytesIO(payload))
            installed = script.extract_binary(
                archive,
                tool="cargo-dylint",
                target="x86_64-unknown-linux-gnu",
                output_dir=root / "bin",
            )
            self.assertEqual(installed, root / "bin" / "cargo-dylint")
            self.assertEqual(installed.read_bytes(), payload)


if __name__ == "__main__":
    unittest.main()
