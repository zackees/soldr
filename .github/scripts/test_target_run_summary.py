import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("target_run_summary.py")
SPEC = importlib.util.spec_from_file_location("target_run_summary", SCRIPT)
assert SPEC and SPEC.loader
target_run_summary = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(target_run_summary)


class TargetRunSummaryTests(unittest.TestCase):
    def write_list(self, path: Path, *, discovered: object, ignored: int = 0) -> None:
        testcases = {
            f"test-{index}": {"ignored": index < ignored}
            for index in range(discovered if isinstance(discovered, int) else 0)
        }
        path.write_text(
            json.dumps(
                {
                    "test-count": discovered,
                    "rust-suites": {"suite": {"testcases": testcases}},
                }
            ),
            encoding="utf-8",
        )

    def test_setup_summary_exists_before_nextest_runs(self) -> None:
        summary = target_run_summary.build_summary("aarch64-apple-darwin")
        self.assertEqual(summary["phase"], "setup")
        self.assertIsNone(summary["discovered"])
        self.assertIsNone(summary["executed"])

    def test_list_and_junit_counts_are_combined(self) -> None:
        with tempfile.TemporaryDirectory() as raw_temp:
            temp = Path(raw_temp)
            test_list = temp / "list.json"
            junit = temp / "junit.xml"
            test_list.write_text(
                json.dumps(
                    {
                        "test-count": 4,
                        "rust-suites": {
                            "suite": {
                                "testcases": {
                                    "passes": {"ignored": False},
                                    "fails": {"ignored": False},
                                    "ignored": {"ignored": True},
                                    "skipped": {"ignored": False},
                                }
                            }
                        },
                    }
                ),
                encoding="utf-8",
            )
            junit.write_text(
                '<testsuites tests="3" failures="1" errors="0" skipped="1" />',
                encoding="utf-8",
            )

            summary = target_run_summary.build_summary(
                "x86_64-unknown-linux-musl", test_list, junit
            )

        self.assertEqual(
            summary,
            {
                "schema_version": 1,
                "target": "x86_64-unknown-linux-musl",
                "phase": "completed",
                "discovered": 4,
                "ignored": 1,
                "executed": 3,
                "passed": 1,
                "failed": 1,
                "skipped": 1,
            },
        )

    def test_junit_suite_children_are_aggregated(self) -> None:
        with tempfile.TemporaryDirectory() as raw_temp:
            junit = Path(raw_temp) / "junit.xml"
            junit.write_text(
                "<testsuites>"
                '<testsuite tests="2" failures="0" errors="0" skipped="0" />'
                '<testsuite tests="3" failures="1" errors="1" skipped="0" />'
                "</testsuites>",
                encoding="utf-8",
            )
            counts = target_run_summary.read_junit(junit)
        self.assertEqual(
            counts, {"executed": 5, "passed": 3, "failed": 2, "skipped": 0}
        )

    def test_completed_summary_requires_junit_when_requested(self) -> None:
        with tempfile.TemporaryDirectory() as raw_temp:
            missing = Path(raw_temp) / "missing.xml"
            with self.assertRaisesRegex(ValueError, "required JUnit report is missing"):
                target_run_summary.build_summary(
                    "x86_64-pc-windows-msvc", junit=missing, require_junit=True
                )

    def test_supplied_list_path_must_exist(self) -> None:
        with tempfile.TemporaryDirectory() as raw_temp:
            missing = Path(raw_temp) / "missing-list.json"
            with self.assertRaisesRegex(ValueError, "nextest list JSON is missing"):
                target_run_summary.read_test_list(missing)

    def test_invalid_junit_shapes_and_totals_are_rejected(self) -> None:
        cases = {
            "wrong-root": '<not-junit tests="2" />',
            "empty-root": "<testsuites />",
            "negative": '<testsuite tests="2" failures="-1" />',
            "over-accounted": '<testsuite tests="2" failures="2" skipped="1" />',
        }
        with tempfile.TemporaryDirectory() as raw_temp:
            path = Path(raw_temp) / "junit.xml"
            for name, xml in cases.items():
                with self.subTest(name=name):
                    path.write_text(xml, encoding="utf-8")
                    with self.assertRaises(ValueError):
                        target_run_summary.read_junit(path)

    def test_discovered_executed_and_ignored_must_reconcile(self) -> None:
        with tempfile.TemporaryDirectory() as raw_temp:
            temp = Path(raw_temp)
            test_list = temp / "list.json"
            junit = temp / "junit.xml"
            self.write_list(test_list, discovered=8, ignored=1)
            junit.write_text('<testsuite tests="1" />', encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "coverage counts disagree"):
                target_run_summary.build_summary(
                    "aarch64-unknown-linux-gnu", test_list, junit
                )

    def test_boolean_discovered_count_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as raw_temp:
            test_list = Path(raw_temp) / "list.json"
            self.write_list(test_list, discovered=True)
            with self.assertRaisesRegex(ValueError, "integer test-count"):
                target_run_summary.read_test_list(test_list)


if __name__ == "__main__":
    unittest.main()
