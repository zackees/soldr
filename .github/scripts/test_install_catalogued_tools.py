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
        response = io.BytesIO(b'{"schema_version": 2, "entries": []}')
        with (
            mock.patch.object(
                script.urllib.request,
                "urlopen",
                side_effect=[urllib.error.URLError("reset"), response],
            ) as urlopen,
            mock.patch.object(script.time, "sleep") as sleep,
        ):
            catalogue = script.fetch_catalogue("https://example.test/catalogue.json")

        self.assertEqual(catalogue["schema_version"], 2)
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
                {"schema_version": 2, "entries": [row]},
                tool="dylint-link",
                version="6.0.3",
                target="x86_64-unknown-linux-gnu",
            ),
            row,
        )
        with self.assertRaisesRegex(SystemExit, "exactly one"):
            script.select_entry(
                {"schema_version": 2, "entries": []},
                tool="dylint-link",
                version="6.0.3",
                target="x86_64-unknown-linux-gnu",
            )
        with self.assertRaisesRegex(SystemExit, "valid sha256"):
            script.select_entry(
                {"schema_version": 2, "entries": [{**row, "sha256": "bad"}]},
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


class MultipartCatalogueTests(unittest.TestCase):
    """soldr#2850's catalogue v2 migration: 150 of the 152 published rows are
    multipart and carry no single URL, so this path is the normal one now."""

    @staticmethod
    def _part(number: int, payload: bytes) -> dict[str, object]:
        return {
            "number": number,
            "sha256": hashlib.sha256(payload).hexdigest(),
            "size_bytes": len(payload),
            "urls": [f"https://parts.test/{number}"],
        }

    def test_a_multipart_row_is_reassembled_in_order(self) -> None:
        chunks = [b"first-half-", b"second-half"]
        whole = b"".join(chunks)
        entry = {
            "asset": "cargo-dylint-6.0.3-x86_64-unknown-linux-gnu.tar.gz",
            "sha256": hashlib.sha256(whole).hexdigest(),
            "parts": [self._part(i, c) for i, c in enumerate(chunks, start=1)],
        }
        with (
            tempfile.TemporaryDirectory() as temp,
            mock.patch.object(
                script,
                "read_url_bytes",
                side_effect=lambda url: chunks[int(url.rsplit("/", 1)[1]) - 1],
            ),
        ):
            output = Path(temp) / "asset.tar.gz"
            script.download_verified(entry, output)
            self.assertEqual(output.read_bytes(), whole)

    def test_a_corrupt_part_is_named_rather_than_surfacing_as_a_whole_file_miss(
        self,
    ) -> None:
        chunks = [b"good-part-", b"bad-part"]
        whole = b"".join(chunks)
        entry = {
            "asset": "cargo-dylint-6.0.3-x86_64-unknown-linux-gnu.tar.gz",
            "sha256": hashlib.sha256(whole).hexdigest(),
            "parts": [self._part(i, c) for i, c in enumerate(chunks, start=1)],
        }
        with (
            tempfile.TemporaryDirectory() as temp,
            mock.patch.object(
                script,
                "read_url_bytes",
                # Part 2 comes back wrong.
                side_effect=lambda url: (
                    chunks[0] if url.endswith("/1") else b"tampered"
                ),
            ),
            self.assertRaisesRegex(SystemExit, "part 2 sha256 mismatch"),
        ):
            script.download_verified(entry, Path(temp) / "asset.tar.gz")

    def test_non_contiguous_parts_are_refused(self) -> None:
        entry = {
            "asset": "cargo-dylint-6.0.3-x86_64-unknown-linux-gnu.tar.gz",
            "sha256": "a" * 64,
            "parts": [self._part(1, b"a"), self._part(3, b"b")],
        }
        with (
            tempfile.TemporaryDirectory() as temp,
            mock.patch.object(script, "read_url_bytes", return_value=b"a"),
            self.assertRaisesRegex(SystemExit, "non-contiguous"),
        ):
            script.download_verified(entry, Path(temp) / "asset.tar.gz")

    def test_a_row_with_neither_urls_nor_parts_is_refused_at_selection(self) -> None:
        expected = script.asset_name("dylint-link", "6.0.3", "x86_64-unknown-linux-gnu")
        with self.assertRaisesRegex(SystemExit, "neither a download URL nor parts"):
            script.select_entry(
                {
                    "schema_version": 2,
                    "entries": [{"asset": expected, "sha256": "a" * 64}],
                },
                tool="dylint-link",
                version="6.0.3",
                target="x86_64-unknown-linux-gnu",
            )

    def test_the_v2_urls_list_is_preferred_over_the_v1_singular(self) -> None:
        entry = {"urls": ["https://new.test/a"], "url": "https://old.test/a"}
        self.assertEqual(script.direct_urls(entry), ["https://new.test/a"])
        self.assertEqual(
            script.direct_urls({"url": "https://old.test/a"}), ["https://old.test/a"]
        )
        self.assertEqual(script.direct_urls({}), [])
