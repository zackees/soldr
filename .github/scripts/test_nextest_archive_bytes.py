import json
import tempfile
import unittest
from pathlib import Path

from _script_loader import load_sibling_script

nextest_archive_bytes = load_sibling_script("nextest_archive_bytes")


def _tree(root: Path) -> Path:
    """A miniature extracted archive: two test binaries plus byproducts."""
    deps = root / "target" / "debug" / "deps"
    deps.mkdir(parents=True)
    (deps / "soldr_cli-abc123").write_bytes(b"0" * 4096)
    (deps / "isolated_daemon-def456.exe").write_bytes(b"0" * 2048)
    (deps / "soldr_cli-abc123.d").write_bytes(b"0" * 16)
    (deps / "libsoldr_core-1.rlib").write_bytes(b"0" * 32)
    (root / "target" / "debug" / "build.log").write_bytes(b"0" * 8)
    return root


class ClassificationTests(unittest.TestCase):
    def test_deps_executables_are_test_binaries(self) -> None:
        self.assertTrue(
            nextest_archive_bytes.is_test_binary(
                Path("target/debug/deps/soldr_cli-abc123")
            )
        )
        self.assertTrue(
            nextest_archive_bytes.is_test_binary(
                Path("target/debug/deps/soldr_cli-abc123.exe")
            )
        )

    def test_byproducts_are_not_test_binaries(self) -> None:
        for name in ["soldr_cli-abc.d", "libx-1.rlib", "x.rmeta", "x.pdb", "x.o"]:
            self.assertFalse(
                nextest_archive_bytes.is_test_binary(Path("target/debug/deps") / name),
                name,
            )

    def test_files_outside_deps_are_not_test_binaries(self) -> None:
        """Packaged `soldr` binaries and build-script outputs ride along in
        the archive; counting them would inflate the count without changing
        the byte story soldr#2931 is asking about."""
        self.assertFalse(
            nextest_archive_bytes.is_test_binary(Path("package/soldr.exe"))
        )


class SummaryTests(unittest.TestCase):
    def test_totals_and_ordering(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            summary = nextest_archive_bytes.summarize(
                nextest_archive_bytes.scan(_tree(Path(raw))), archive_bytes=1024
            )
        self.assertEqual(summary["test_binary_count"], 2)
        self.assertEqual(summary["test_binary_bytes"], 4096 + 2048)
        self.assertEqual(summary["file_count"], 5)
        sizes = [entry["size"] for entry in summary["test_binaries"]]
        self.assertEqual(sizes, sorted(sizes, reverse=True))
        self.assertGreater(summary["extracted_over_archive"], 0)

    def test_an_empty_tree_does_not_divide_by_zero(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            summary = nextest_archive_bytes.summarize(
                nextest_archive_bytes.scan(Path(raw)), archive_bytes=None
            )
        self.assertEqual(summary["extracted_bytes"], 0)
        self.assertIsNone(summary["extracted_over_archive"])
        self.assertIsNone(summary["test_binary_share"])


class MarkdownTests(unittest.TestCase):
    def test_the_step_summary_names_the_binaries(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            summary = nextest_archive_bytes.summarize(
                nextest_archive_bytes.scan(_tree(Path(raw))), archive_bytes=1024
            )
        rendered = nextest_archive_bytes.render_markdown(summary, top=10)
        self.assertIn("Nextest archive byte attribution", rendered)
        self.assertIn("Test binaries", rendered)
        self.assertIn("soldr_cli-abc123", rendered)


class MainTests(unittest.TestCase):
    def test_main_writes_json_and_summary_and_never_fails(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            _tree(root / "extracted")
            archive = root / "tests.tar.zst"
            archive.write_bytes(b"0" * 512)
            report = root / "bytes.json"
            step_summary = root / "summary.md"
            status = nextest_archive_bytes.main(
                [
                    "--extract-dir",
                    str(root / "extracted"),
                    "--archive",
                    str(archive),
                    "--json",
                    str(report),
                    "--summary",
                    str(step_summary),
                ]
            )
            self.assertEqual(status, 0)
            payload = json.loads(report.read_text(encoding="utf-8"))
            self.assertEqual(payload["test_binary_count"], 2)
            self.assertIn("byte attribution", step_summary.read_text(encoding="utf-8"))

    def test_a_missing_extraction_directory_is_not_fatal(self) -> None:
        """The diagnostic runs with `always()` after a run that may have died
        before extraction. It must never be the reason a lane is red."""
        self.assertEqual(
            nextest_archive_bytes.main(["--extract-dir", "/definitely/not/here"]), 0
        )

    def test_an_empty_extract_dir_argument_is_not_fatal(self) -> None:
        self.assertEqual(nextest_archive_bytes.main(["--extract-dir", ""]), 0)


if __name__ == "__main__":
    unittest.main()
