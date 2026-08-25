#!/usr/bin/env python3
"""Unit tests for toolchain_asset_query.py."""

from __future__ import annotations

import hashlib
import json
import unittest
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
                ],
            }
        ],
    }


class ToolchainAssetQueryTests(unittest.TestCase):
    def test_resolve_metadata_follows_verified_root_descriptor(self) -> None:
        catalog = json.dumps(sample_manifest()).encode()
        index = json.dumps(
            {
                "kind": "Index",
                "schema_version": 1,
                "tools": {
                    "demo": {
                        "descriptor": {
                            "url": "generations/source-test/demo/manifest.json",
                            "size_bytes": len(catalog),
                            "sha256": hashlib.sha256(catalog).hexdigest(),
                        }
                    }
                },
            }
        ).encode()
        with mock.patch.object(taq, "fetch_bytes", side_effect=[index, catalog]) as fetch:
            metadata = taq.resolve_metadata(
                tool="demo",
                origin="https://example.test/catalogue",
                tool_manifest_url_override=None,
                platform="linux",
                arch="x86_64",
                extra="gnu",
                version="1.2.3",
            )

        self.assertEqual(
            [call.args[0] for call in fetch.call_args_list],
            [
                "https://example.test/catalogue/manifest.json",
                "https://example.test/catalogue/generations/source-test/demo/manifest.json",
            ],
        )
        self.assertEqual(metadata["sha256"], "0" * 64)

    def test_root_descriptor_digest_mismatch_fails_closed(self) -> None:
        index = json.dumps(
            {
                "schema_version": 1,
                "tools": {
                    "demo": {
                        "descriptor": {
                            "url": "demo/manifest.json",
                            "size_bytes": 2,
                            "sha256": "0" * 64,
                        }
                    }
                }
            }
        ).encode()
        with (
            mock.patch.object(taq, "fetch_bytes", side_effect=[index, b"{}"]),
            self.assertRaisesRegex(SystemExit, "sha256 mismatch"),
        ):
            taq.load_tool_manifest("https://example.test", "demo")

    def test_single_multipart_chunk_is_a_direct_download_equivalent(self) -> None:
        release = taq.find_release(sample_manifest(), "latest")
        asset = release["platforms"][0]["asset"]
        asset["parts"] = [
            {
                "number": 1,
                "size_bytes": asset["size_bytes"],
                "sha256": asset["sha256"],
                "urls": ["https://example.test/part"],
            }
        ]
        del asset["urls"]

        selected = taq.find_asset(
            release, taq.platform_candidates("linux", "x86_64", "gnu")
        )
        self.assertEqual(selected["urls"], ["https://example.test/part"])

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
            release, taq.platform_candidates("darwin", "aarch64", None)
        )
        self.assertTrue(url.endswith("demo-darwin-universal2.tar.gz"))

    def test_json_metadata_includes_digest_and_platform(self) -> None:
        metadata = taq.find_asset(
            taq.find_release(sample_manifest(), "latest"),
            taq.platform_candidates("linux", "x86_64", "gnu"),
        )
        self.assertEqual(metadata["sha256"], "0" * 64)
        self.assertEqual(metadata["platform"]["libc"], "glibc")


if __name__ == "__main__":
    unittest.main()
