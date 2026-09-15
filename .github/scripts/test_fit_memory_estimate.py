"""Tests for fit_memory_estimate.py (soldr#3152 step 2/3)."""

import contextlib
import io
import json
import tempfile
import unittest
from pathlib import Path

from _script_loader import load_sibling_script

fit = load_sibling_script("fit_memory_estimate")

# The same value crates/soldr-daemon/src/memory_estimate_tests.rs pins, so the
# Python join key and the daemon's cannot drift apart.
PINNED_DIGEST = "24bd102264f931a80336bf1378360965954b9c77d2ab1576c44cebda09e61176"


def _write_jsonl(path: Path, rows: list[dict]) -> None:
    with path.open("w", encoding="utf-8") as handle:
        for row in rows:
            handle.write(json.dumps(row) + "\n")


def _estimate(args: list[str], estimate: int, **extra) -> dict:
    row = {
        "schema_version": 1,
        "args_digest": fit.args_digest(args),
        "crate_name": extra.pop("crate_name", None),
        "estimate_bytes": estimate,
        "exclusive": extra.pop("exclusive", False),
        "is_test": False,
        "lto": "off",
    }
    row.update(extra)
    return row


def _journal(args: list[str], outcome: str, peak: int | None) -> dict:
    row: dict[str, object] = {
        "outcome": outcome,
        "compiler": "/r/rustc",
        "args": args,
        "cwd": "/w",
    }
    if peak is not None:
        row["child_peak_rss_bytes"] = peak
    return row


class ArgsDigestTests(unittest.TestCase):
    def test_matches_the_daemon_digest(self) -> None:
        self.assertEqual(fit.args_digest(["--crate-name", "unit"]), PINNED_DIGEST)

    def test_nul_separation_keeps_boundaries_distinct(self) -> None:
        self.assertNotEqual(fit.args_digest(["ab", "c"]), fit.args_digest(["a", "bc"]))


class UnitKeyTests(unittest.TestCase):
    def test_matches_the_daemon_unit_key_for_both_spellings(self) -> None:
        for args in (
            ["--crate-name", "thiserror", "-C", "metadata=a811629650414cac"],
            ["--crate-name=thiserror", "-Cmetadata=a811629650414cac"],
        ):
            self.assertEqual(fit.unit_key(args), "thiserror/a811629650414cac")
        self.assertIsNone(fit.unit_key(["--crate-name", "thiserror"]))

    def test_unit_key_joins_when_admission_saw_rewritten_args(self) -> None:
        # zccache journals the client's args but admits a rewritten vector, so
        # the digests differ. The unit key must still pair them.
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            journaled = ["--crate-name", "anyhow", "-C", "metadata=0123456789abcdef"]
            admitted = [*journaled, "--remap-path-prefix", "/w=."]
            row = _estimate(admitted, 100, crate_name="anyhow")
            row["schema_version"] = 2
            row["unit_key"] = "anyhow/0123456789abcdef"
            _write_jsonl(root / "admission-estimate.jsonl", [row])
            _write_jsonl(
                root / "compile_journal.jsonl", [_journal(journaled, "miss", 400)]
            )
            report = fit.analyze([root / "admission-estimate.jsonl"], [root])
            self.assertEqual(report["joined"], 1)
            self.assertEqual(report["unjoined"], 0)
            self.assertEqual(report["under_predicted"], 1)


class JoinTests(unittest.TestCase):
    def setUp(self) -> None:
        self._stack = contextlib.ExitStack()
        self.root = Path(self._stack.enter_context(tempfile.TemporaryDirectory()))
        self.heavy = ["--crate-name", "broker", "--test"]
        self.light = ["--crate-name", "tap"]
        self.unmeasured = ["--crate-name", "hit_only"]
        self.orphan = ["--crate-name", "never_journaled"]
        _write_jsonl(
            self.root / "admission-estimate.jsonl",
            [
                _estimate(self.heavy, 1_000, crate_name="broker", exclusive=True),
                _estimate(self.light, 900, crate_name="tap"),
                _estimate(self.unmeasured, 500, crate_name="hit_only"),
                _estimate(self.orphan, 500, crate_name="never_journaled"),
            ],
        )
        logs = self.root / "cache" / "zccache" / "logs"
        logs.mkdir(parents=True)
        _write_jsonl(
            logs / "compile_journal.jsonl",
            [
                _journal(self.heavy, "miss", 4_000),
                _journal(self.light, "miss", 300),
                _journal(self.unmeasured, "hit", None),
            ],
        )

    def tearDown(self) -> None:
        self._stack.close()

    def test_pairs_each_estimate_with_the_measured_compile(self) -> None:
        report = fit.analyze([self.root / "admission-estimate.jsonl"], [self.root])
        self.assertEqual(report["estimates"], 4)
        self.assertEqual(report["joined"], 2)
        self.assertEqual(report["unmeasured"], 1)
        self.assertEqual(report["unjoined"], 1)

    def test_under_prediction_is_counted_and_ranked(self) -> None:
        report = fit.analyze([self.root / "admission-estimate.jsonl"], [self.root])
        self.assertEqual(report["under_predicted"], 1)
        worst = report["worst_under_predictions"][0]
        self.assertEqual(worst["crate_name"], "broker")
        self.assertEqual(worst["measured_bytes"], 4_000)
        self.assertEqual(worst["estimate_bytes"], 1_000)
        self.assertAlmostEqual(worst["measured_over_estimate"], 4.0)
        self.assertEqual(report["max_measured_over_estimate"], 4.0)

    def test_cli_emits_the_report_as_json(self) -> None:
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = fit.main(
                [
                    "--estimates",
                    str(self.root / "admission-estimate.jsonl"),
                    "--journal",
                    str(self.root),
                    "--json",
                ]
            )
        self.assertEqual(code, 0)
        self.assertEqual(json.loads(out.getvalue())["joined"], 2)


if __name__ == "__main__":
    unittest.main()
