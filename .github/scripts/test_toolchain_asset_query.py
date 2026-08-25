#!/usr/bin/env python3
"""Unit tests for toolchain_asset_query.py."""

from __future__ import annotations

import unittest
import urllib.error
from pathlib import Path
from unittest import mock

from _script_loader import load_script_module

SCRIPT = Path(__file__).with_name("toolchain_asset_query.py")
taq = load_script_module(SCRIPT, "toolchain_asset_query")


def sample_manifest() -> dict:
    return {
        "kind": "Catalog",
        "tool": "demo",
        "channels": {"latest-stable": "v1.2.3"},
        "releases": [
            {
                "version": "v1.2.3",
                "platforms": [
                    {
                        "platform": {"os": "linux", "arch": "x86_64", "libc": "glibc"},
                        "asset": {
                            "filename": "demo-linux-gnu.tar.xz",
                            "size_bytes": 1,
                            "sha256": "0" * 64,
                            "urls": ["https://example.test/demo-linux-gnu.tar.xz"],
                        },
                    },
                    {
                        "platform": {"os": "linux", "arch": "x86_64", "libc": "musl"},
                        "asset": {
                            "filename": "demo-linux-musl.tar.xz",
                            "size_bytes": 1,
                            "sha256": "1" * 64,
                            "urls": ["https://example.test/demo-linux-musl.tar.xz"],
                        },
                    },
                    {
                        "platform": {"os": "windows", "arch": "x86_64", "abi": "msvc"},
                        "asset": {
                            "filename": "demo-windows.zip",
                            "size_bytes": 1,
                            "sha256": "2" * 64,
                            "urls": ["https://example.test/demo-windows.zip"],
                        },
                    },
                    {
                        "platform": {"os": "darwin", "arch": "universal2"},
                        "asset": {
                            "filename": "demo-darwin-universal2.tar.gz",
                            "size_bytes": 1,
                            "sha256": "3" * 64,
                            "urls": [
                                "https://example.test/demo-darwin-universal2.tar.gz"
                            ],
                        },
                    },
                    {
                        "platform": {"os": "darwin", "arch": "aarch64"},
                        "asset": {
                            "filename": "demo-darwin-arm64.tar.gz",
                            "size_bytes": 3,
                            "sha256": "4" * 64,
                            "parts": [
                                {
                                    "number": 1,
                                    "size_bytes": 1,
                                    "sha256": "5" * 64,
                                    "urls": ["https://example.test/part-1"],
                                },
                                {
                                    "number": 2,
                                    "size_bytes": 2,
                                    "sha256": "6" * 64,
                                    "urls": ["https://example.test/part-2"],
                                },
                            ],
                        },
                    },
                ],
            }
        ],
    }


class ToolchainAssetQueryTests(unittest.TestCase):
    def test_latest_uses_channel(self) -> None:
        release = taq.find_release(sample_manifest(), "latest")
        self.assertEqual(release["version"], "v1.2.3")

    def test_version_accepts_bare_or_v_prefixed(self) -> None:
        payload = sample_manifest()
        self.assertEqual(taq.find_release(payload, "1.2.3")["version"], "v1.2.3")
        self.assertEqual(taq.find_release(payload, "v1.2.3")["version"], "v1.2.3")

    def test_linux_gnu_prefers_glibc_then_musl(self) -> None:
        candidates = taq.platform_candidates("linux", "x86_64", "gnu")
        self.assertEqual(
            candidates,
            [
                {"os": "linux", "arch": "x86_64", "libc": "glibc"},
                {"os": "linux", "arch": "x86_64", "libc": "musl"},
            ],
        )

    def test_default_linux_prefers_glibc(self) -> None:
        release = taq.find_release(sample_manifest(), "latest")
        url = taq.find_asset_url(
            release, taq.platform_candidates("linux", "x86_64", None)
        )
        self.assertTrue(url.endswith("demo-linux-gnu.tar.xz"))

    def test_darwin_arch_can_fallback_to_universal2(self) -> None:
        release = taq.find_release(sample_manifest(), "latest")
        url = taq.find_asset_url(
            release, taq.platform_candidates("darwin", "x86_64", None)
        )
        self.assertTrue(url.endswith("demo-darwin-universal2.tar.gz"))

    def test_json_metadata_includes_digest_and_platform(self) -> None:
        metadata = taq.find_asset(
            taq.find_release(sample_manifest(), "latest"),
            taq.platform_candidates("linux", "x86_64", "gnu"),
        )
        self.assertEqual(metadata["sha256"], "0" * 64)
        self.assertEqual(metadata["platform"]["libc"], "glibc")

    def test_json_metadata_preserves_multipart_transport(self) -> None:
        metadata = taq.find_asset(
            taq.find_release(sample_manifest(), "latest"),
            taq.platform_candidates("darwin", "aarch64", None),
        )
        self.assertEqual(metadata["urls"], [])
        self.assertEqual([part["number"] for part in metadata["parts"]], [1, 2])
        with self.assertRaisesRegex(SystemExit, "multipart"):
            taq.find_asset_url(
                taq.find_release(sample_manifest(), "latest"),
                taq.platform_candidates("darwin", "aarch64", None),
            )

    def test_metadata_rejects_filename_escape_shapes(self) -> None:
        release = taq.find_release(sample_manifest(), "latest")
        asset = release["platforms"][0]["asset"]
        for filename in (
            "../outside.tar.gz",
            "/tmp/outside.tar.gz",
            "nested/outside.tar.gz",
            "nested\\outside.zip",
            "C:\\outside.zip",
            ".",
            "..",
        ):
            asset["filename"] = filename
            with self.assertRaisesRegex(SystemExit, "unsafe filename"):
                taq.find_asset(
                    release,
                    taq.platform_candidates("linux", "x86_64", "gnu"),
                )

    def test_manifest_fetch_rejects_unsafe_url_before_network(self) -> None:
        for url in (
            "http://example.test/manifest.json",
            "https://user:secret@example.test/manifest.json",
        ):
            with self.assertRaisesRegex(SystemExit, "credential-free absolute HTTPS"):
                taq.fetch_json(url)

    def test_manifest_fetch_error_redacts_signed_query(self) -> None:
        secret = "manifest-token-must-not-leak"
        url = f"https://example.test/manifest.json?token={secret}"
        with (
            mock.patch.object(
                taq,
                "open_url",
                side_effect=urllib.error.URLError(f"transport included {secret}"),
            ),
            self.assertRaisesRegex(SystemExit, "network error") as raised,
        ):
            taq.fetch_json(url)
        self.assertNotIn(secret, str(raised.exception))


if __name__ == "__main__":
    unittest.main()
