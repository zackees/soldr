#!/usr/bin/env python3
"""Pure tests for target mapping and archive safety."""

from __future__ import annotations

import importlib.util
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest import mock


def load(name: str):
    path = Path(__file__).with_name(f"{name}.py")
    spec = importlib.util.spec_from_file_location(name, path)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


script = load("fetch_catalogued_nextest")


class FetchNextestTests(unittest.TestCase):
    def test_catalogue_lookup_normalizes_bare_and_prefixed_versions(self) -> None:
        fixture = {
            "version": "0.9.140",
            "filename": "cargo-nextest.zip",
            "urls": ["https://example.test/cargo-nextest.zip"],
            "sha256": "0" * 64,
        }
        for supplied in ["0.9.140", "v0.9.140", "cargo-nextest-0.9.140"]:
            with (
                self.subTest(supplied=supplied),
                mock.patch.object(
                    script, "resolve_metadata", return_value=fixture
                ) as resolve,
            ):
                self.assertIs(
                    script.resolve_catalogued_metadata(
                        target="aarch64-pc-windows-msvc",
                        version=supplied,
                        origin="https://example.test",
                    ),
                    fixture,
                )
                self.assertEqual(
                    resolve.call_args.kwargs["version"],
                    "0.9.140",
                )

    def test_all_supported_targets_have_explicit_catalogue_mapping(self) -> None:
        targets = [
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
            "x86_64-unknown-linux-musl",
            "aarch64-unknown-linux-musl",
            "x86_64-apple-darwin",
            "aarch64-apple-darwin",
            "x86_64-pc-windows-msvc",
            "aarch64-pc-windows-msvc",
        ]
        self.assertEqual(
            len({script.query_for_target(target) for target in targets}), 8
        )

    def test_unknown_target_fails(self) -> None:
        with self.assertRaises(SystemExit):
            script.query_for_target("x86_64-unknown-freebsd")

    def test_safe_member_rejects_absolute_and_parent_paths(self) -> None:
        self.assertTrue(script.safe_member("cargo-nextest/cargo-nextest"))
        self.assertFalse(script.safe_member("../cargo-nextest"))
        self.assertFalse(script.safe_member("/tmp/cargo-nextest"))

    def test_extract_verified_rejects_tar_symlinks(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            archive = root / "asset.tar.gz"
            with tarfile.open(archive, "w:gz") as handle:
                link = tarfile.TarInfo("cargo-nextest")
                link.type = tarfile.SYMTYPE
                link.linkname = "outside"
                handle.addfile(link)
            with self.assertRaisesRegex(SystemExit, "unsafe path"):
                script.extract_verified(archive, root / "out")


if __name__ == "__main__":
    unittest.main()
