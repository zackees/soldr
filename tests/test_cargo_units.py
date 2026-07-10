"""Tests for the exact Cargo Fresh/Dirty measurement oracle."""

import json
from pathlib import Path
import subprocess
import sys

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / "perf" / "lib" / "cargo_units.py"


def write_log(path: Path, *, first_party_fresh: bool) -> None:
    messages = [
        {"reason": "compiler-artifact", "package_id": "dep", "fresh": True},
        {
            "reason": "compiler-artifact",
            "package_id": "root",
            "fresh": first_party_fresh,
        },
        {"reason": "build-finished", "success": True},
    ]
    path.write_text("".join(json.dumps(item) + "\n" for item in messages))


def run(log: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [
            sys.executable,
            str(SCRIPT),
            str(log),
            "--root-package-id",
            "root",
            "--expect-first-party-dirty",
            "1",
        ],
        check=False,
        capture_output=True,
        text=True,
    )


def test_reports_exact_fresh_dirty_and_compiler_counts(tmp_path: Path) -> None:
    log = tmp_path / "cargo.jsonl"
    write_log(log, first_party_fresh=False)

    result = run(log)

    assert result.returncode == 0
    assert json.loads(result.stdout) == {
        "compiler_invocations": 1,
        "dirty_units": 1,
        "first_party_dirty_units": 1,
        "fresh_units": 1,
    }


def test_rejects_intentionally_injected_false_fresh_result(tmp_path: Path) -> None:
    log = tmp_path / "cargo.jsonl"
    write_log(log, first_party_fresh=True)

    result = run(log)

    assert result.returncode == 1
    assert "expected exactly 1 dirty first-party unit(s); got 0" in result.stderr
