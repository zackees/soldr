#!/usr/bin/env python3
"""Unit tests for ``build_asset_index.py``.

Pure local + ``--offline`` tests — no github.com network dependency.
The SHA256SUMS HTTP path is exercised by the live nightly workflow,
not the test suite (a test that requires github.com reachability is a
flaky test).

Run::

    python3 -m unittest .github/scripts/test_build_asset_index.py -v
"""

from __future__ import annotations

import hashlib
import json
import tempfile
import unittest
from pathlib import Path

# Local import — the test lives next to the module under test.
import sys
sys.path.insert(0, str(Path(__file__).resolve().parent))
import build_asset_index as bai  # noqa: E402


class Sha256OfFileTest(unittest.TestCase):

    def test_matches_hashlib_directly(self) -> None:
        """``sha256_of_file`` must agree with hashlib on a known input.

        Asserts the lowercase-hex contract (which the rust-side
        ``sha256_of`` also follows) so a future refactor to chunked
        I/O cannot silently break parity.
        """
        with tempfile.TemporaryDirectory() as tmp:
            f = Path(tmp) / "hello.txt"
            payload = b"hello world\n"
            f.write_bytes(payload)
            expected = hashlib.sha256(payload).hexdigest()
            self.assertEqual(bai.sha256_of_file(f), expected)
            self.assertEqual(bai.sha256_of_file(f), bai.sha256_of_file(f).lower())

    def test_empty_file_sha(self) -> None:
        """SHA-256 of the empty string is the well-known constant
        ``e3b0c44...b7852b855``. This pairs the python sha256_of_file
        check with the rust-side ``sha256_of_matches_expected_digest``
        test in ``trust.rs``.
        """
        with tempfile.TemporaryDirectory() as tmp:
            f = Path(tmp) / "empty"
            f.write_bytes(b"")
            self.assertEqual(
                bai.sha256_of_file(f),
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            )


class ParseSha256SumsTest(unittest.TestCase):

    def test_strips_dot_slash_prefix(self) -> None:
        text = (
            "deadbeef" + "0" * 56 + "  ./foo.tar.gz\n"
            + "cafebabe" + "0" * 56 + "  bar.zip\n"
        )
        parsed = bai.parse_sha256sums(text)
        self.assertEqual(set(parsed.keys()), {"foo.tar.gz", "bar.zip"})

    def test_skips_self_and_installers_and_debug(self) -> None:
        text = (
            "0" * 64 + "  SHA256SUMS\n"
            + "1" * 64 + "  install.sh\n"
            + "2" * 64 + "  install.ps1\n"
            + "3" * 64 + "  zccache-v1.12.9-x86_64-pc-windows-msvc-debug.zip\n"
            + "4" * 64 + "  zccache-v1.12.9-x86_64-pc-windows-msvc.zip\n"
        )
        parsed = bai.parse_sha256sums(text)
        self.assertEqual(
            set(parsed.keys()),
            {"zccache-v1.12.9-x86_64-pc-windows-msvc.zip"},
        )

    def test_rejects_malformed_lines(self) -> None:
        text = (
            "not-a-hash  some.zip\n"
            + "# comment line\n"
            + "\n"
            + ("a" * 64) + "  good.zip\n"
        )
        parsed = bai.parse_sha256sums(text)
        self.assertEqual(parsed, {"good.zip": "a" * 64})


class BuildAssetIndexTest(unittest.TestCase):

    def _make_tree(self, root: Path, *, with_deps_manifest: bool = True) -> bytes:
        """Lay out a fake manifest-branch tree under ``root``.

        Returns the bytes of the vendored ``deps/foo/bar.tar.zst`` file
        so the test can assert the sha matches what was on disk.
        """
        # Vendored asset.
        deps_dir = root / "deps" / "foo"
        deps_dir.mkdir(parents=True)
        payload = b"\x28\xb5\x2f\xfd" + b"hello-vendored-payload\n" * 4
        (deps_dir / "bar.tar.zst").write_bytes(payload)

        # Companion per-area manifest that attributes the vendored
        # asset to a fake (owner, repo, tag).
        if with_deps_manifest:
            deps_manifest = [
                {
                    "tool": "test-vendored",
                    "owner": "vendored",
                    "repo": "test/vendored",
                    "tag": "FooBar-1.0",
                    "assets": {
                        "bar.tar.zst": {
                            "url": "https://example.invalid/bar.tar.zst",
                            "size": len(payload),
                        }
                    },
                }
            ]
            (deps_dir / "manifest.json").write_text(
                json.dumps(deps_manifest, indent=2),
                encoding="utf-8",
            )

        # A per-tool root manifest (e.g. ``zccache/manifest.json``)
        # whose release has no ``SHA256SUMS`` — should contribute zero
        # entries even when ``--offline=False`` (because the body fetch
        # is the only path that produces shas for that tool).
        tool_dir = root / "fake-tool"
        tool_dir.mkdir()
        (tool_dir / "manifest.json").write_text(
            json.dumps([
                {
                    "tool": "fake-tool",
                    "owner": "someone",
                    "repo": "fake-tool",
                    "tag": "v0.0.1",
                    "assets": {
                        "fake-tool-linux.tar.gz": {
                            "url": "https://example.invalid/fake-tool-linux.tar.gz",
                            "size": 1,
                        }
                    },
                }
            ], indent=2),
            encoding="utf-8",
        )

        return payload

    def test_schema_version_and_entry_shape(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            payload = self._make_tree(root)
            expected_sha = hashlib.sha256(payload).hexdigest()

            index = bai.build_asset_index(
                root,
                repo_owner="zackees",
                repo_name="soldr",
                offline=True,
            )

            self.assertEqual(
                index["schema_version"],
                bai.ASSET_INDEX_SCHEMA_VERSION,
            )

            # Find the vendored entry by asset name. The deps-area
            # manifest attributes it to (vendored, test/vendored,
            # FooBar-1.0) — assert that matches.
            vendored = [
                e for e in index["entries"]
                if e["asset"] == "bar.tar.zst"
                and e["owner"] == "vendored"
            ]
            self.assertEqual(len(vendored), 1, msg=index)
            entry = vendored[0]
            self.assertEqual(entry["repo"], "test/vendored")
            self.assertEqual(entry["tag"], "FooBar-1.0")
            self.assertEqual(entry["sha256"], expected_sha)
            self.assertEqual(
                entry["url"],
                "https://media.githubusercontent.com/media/zackees/soldr/manifest/"
                "deps/foo/bar.tar.zst",
            )
            self.assertEqual(len(entry["sha256"]), 64)

    def test_self_attributes_unowned_deps_files(self) -> None:
        """A deps/ file that ISN'T named in any per-area manifest's
        assets map should still appear in the index, self-attributed
        to (<repo_owner>, <repo_name>, "manifest", <rel-path>) so the
        resolver could ask for it explicitly if needed."""
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self._make_tree(root, with_deps_manifest=False)

            index = bai.build_asset_index(
                root,
                repo_owner="zackees",
                repo_name="soldr",
                offline=True,
            )

            # No deps manifest attribution → both the .tar.zst and
            # (if present) the manifest.json get the self-attributed
            # shape.
            self_attributed = [
                e for e in index["entries"]
                if e["owner"] == "zackees" and e["repo"] == "soldr"
                and e["tag"] == "manifest"
            ]
            assets = {e["asset"] for e in self_attributed}
            self.assertIn("deps/foo/bar.tar.zst", assets)

    def test_offline_skips_release_entries(self) -> None:
        """``offline=True`` must not produce GitHub-Releases entries
        even when a per-tool manifest names a SHA256SUMS asset."""
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self._make_tree(root)

            # Add a per-tool manifest that NAMES a SHA256SUMS asset.
            # The script must NOT fetch it under --offline.
            (root / "evil-tool").mkdir()
            (root / "evil-tool" / "manifest.json").write_text(
                json.dumps([
                    {
                        "tool": "evil",
                        "owner": "evil",
                        "repo": "evil",
                        "tag": "v1.0.0",
                        "assets": {
                            "SHA256SUMS": {
                                "url": "https://example.invalid/SHA256SUMS",
                                "size": 1,
                            },
                            "evil-payload.zip": {
                                "url": "https://example.invalid/evil-payload.zip",
                                "size": 1,
                            },
                        },
                    }
                ], indent=2),
                encoding="utf-8",
            )

            index = bai.build_asset_index(
                root,
                repo_owner="zackees",
                repo_name="soldr",
                offline=True,
            )
            evil_entries = [
                e for e in index["entries"]
                if e["owner"] == "evil" or e["asset"].startswith("evil-")
            ]
            self.assertEqual(evil_entries, [], msg=index)

    def test_entries_sorted_deterministically(self) -> None:
        """The output entries must be sorted ascending by
        (owner, repo, tag, asset) so two runs produce byte-identical
        files when the inputs match — the whole point of running
        ``write_if_changed`` in the nightly workflow.
        """
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self._make_tree(root)

            # Add more vendored files (out of name order) to provoke
            # a sort.
            d = root / "deps" / "zzz"
            d.mkdir(parents=True)
            (d / "z-asset.bin").write_bytes(b"zzz")
            d2 = root / "deps" / "aaa"
            d2.mkdir(parents=True)
            (d2 / "a-asset.bin").write_bytes(b"aaa")

            index = bai.build_asset_index(
                root,
                repo_owner="zackees",
                repo_name="soldr",
                offline=True,
            )
            keys = [
                (e["owner"], e["repo"], e["tag"], e["asset"])
                for e in index["entries"]
            ]
            self.assertEqual(keys, sorted(keys))

    def test_cli_writes_output_file(self) -> None:
        """The argparse entrypoint must write the JSON to --output."""
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self._make_tree(root)
            out = Path(tmp) / "out" / "asset-index.json"
            rc = bai.main([
                "--manifest-checkout", str(root),
                "--output", str(out),
                "--offline",
            ])
            self.assertEqual(rc, 0)
            self.assertTrue(out.is_file())
            payload = json.loads(out.read_text(encoding="utf-8"))
            self.assertIn("entries", payload)
            self.assertEqual(
                payload["schema_version"],
                bai.ASSET_INDEX_SCHEMA_VERSION,
            )


class CliHelpTest(unittest.TestCase):

    def test_help_text_mentions_asset_index(self) -> None:
        """``--help`` shouldn't crash and should name the file we emit
        somewhere in the doc text so a human reading ``--help`` can
        confirm they invoked the right script.
        """
        import io
        import contextlib

        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            try:
                bai.main(["--help"])
            except SystemExit as e:
                # argparse exits with 0 on --help; anything else is a
                # bug in the parser definition.
                self.assertEqual(e.code, 0)
        self.assertIn("asset-index.json", buf.getvalue())


if __name__ == "__main__":
    unittest.main(verbosity=2)
