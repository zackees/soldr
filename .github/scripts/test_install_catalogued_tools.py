#!/usr/bin/env python3
"""Tests for the hash-verified catalogue tool installer."""

from __future__ import annotations

import hashlib
import io
import json
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
    def test_catalogued_entry_uses_verified_tool_descriptor(self) -> None:
        metadata = {
            "filename": "cargo-dylint-6.0.3-x86_64-unknown-linux-gnu.tar.gz",
            "urls": ["https://example.test/cargo-dylint.tar.gz"],
            "sha256": "a" * 64,
        }
        with mock.patch.object(
            script.toolchain_asset_query, "resolve_metadata", return_value=metadata
        ) as resolve:
            entry = script.catalogued_entry(
                origin="https://example.test/catalogue",
                tool="cargo-dylint",
                version="6.0.3",
                target="x86_64-unknown-linux-gnu",
            )

        self.assertEqual(entry["asset"], metadata["filename"])
        self.assertEqual(entry["url"], metadata["urls"][0])
        resolve.assert_called_once_with(
            tool="cargo-dylint",
            origin="https://example.test/catalogue",
            tool_manifest_url_override=None,
            platform="linux",
            arch="x86_64",
            extra="gnu",
            version="6.0.3",
        )

    def test_catalogued_entry_retries_transient_root_fetch(self) -> None:
        asset = {
            "filename": "cargo-dylint-6.0.3-x86_64-unknown-linux-gnu.tar.gz",
            "size_bytes": 123,
            "sha256": "a" * 64,
            "urls": ["https://example.test/cargo-dylint.tar.gz"],
        }
        catalog = json.dumps(
            {
                "schema_version": 1,
                "releases": [
                    {
                        "version": "v6.0.3",
                        "platforms": [
                            {
                                "platform": {
                                    "os": "linux",
                                    "arch": "x86_64",
                                    "libc": "glibc",
                                },
                                "asset": asset,
                            }
                        ],
                    }
                ],
            }
        ).encode()
        index = json.dumps(
            {
                "schema_version": 1,
                "tools": {
                    "cargo-dylint": {
                        "descriptor": {
                            "url": "generation/catalog.json",
                            "size_bytes": len(catalog),
                            "sha256": hashlib.sha256(catalog).hexdigest(),
                        }
                    }
                },
            }
        ).encode()
        query = script.toolchain_asset_query
        with (
            mock.patch.object(
                query.urllib.request,
                "urlopen",
                side_effect=[
                    urllib.error.URLError("reset"),
                    io.BytesIO(index),
                    io.BytesIO(catalog),
                ],
            ) as urlopen,
            mock.patch.object(query.time, "sleep") as sleep,
        ):
            entry = script.catalogued_entry(
                origin="https://example.test",
                tool="cargo-dylint",
                version="6.0.3",
                target="x86_64-unknown-linux-gnu",
            )

        self.assertEqual(entry["sha256"], "a" * 64)
        self.assertEqual(urlopen.call_count, 3)
        sleep.assert_called_once_with(query.RETRY_BASE_DELAY_SECS)

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


class MsvcLinkerSmokeTest(unittest.TestCase):
    """`dylint-link` on MSVC has no version surface of its own.

    It forwards to `link.exe`, which rejects `--version` and exits non-zero,
    so an exit-code-only smoke can never pass on Windows -- the install
    aborts and the tool is left uninstalled.
    """

    MSVC_FAILURE = "\n".join(
        (
            "Microsoft (R) Incremental Linker Version 14.51.36252.0",
            "LINK : warning LNK4044: unrecognized option '/-version'; ignored",
            "LINK : fatal error LNK1561: entry point must be defined",
        )
    )

    def test_reaching_the_msvc_linker_counts_as_smoked(self) -> None:
        self.assertTrue(
            script._delegated_to_msvc_linker("dylint-link", self.MSVC_FAILURE)
        )

    def test_only_dylint_link_gets_the_exemption(self) -> None:
        """A real tool failing must never be excused by linker output."""
        self.assertFalse(
            script._delegated_to_msvc_linker("cargo-dylint", self.MSVC_FAILURE)
        )

    def test_unrecognisable_output_is_still_a_failure(self) -> None:
        """The banner is the evidence; without it nothing was established."""
        for output in ("", "command not found", "Segmentation fault"):
            self.assertFalse(script._delegated_to_msvc_linker("dylint-link", output))

    def test_unix_dylint_link_is_unaffected(self) -> None:
        """On Unix it forwards to `cc`, which answers `--version` properly."""
        self.assertFalse(
            script._delegated_to_msvc_linker("dylint-link", "cc (GCC) 13.2.0")
        )


if __name__ == "__main__":
    unittest.main()
