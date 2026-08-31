#!/usr/bin/env python3
"""Run the fixed cold Cargo orchestration evidence matrix (soldr#2878).

This is deliberately a small dispatch-workflow adapter around
``cargo_orchestration_telemetry.py``.  It fixes the comparison at N=1, N=2,
and N=8, while the lower-level runner owns fresh case directories, cgroup
sampling, command logs, and the failure result.  Keeping that control flow in
Python lets the Actions workflow remain an auditable setup/execute/upload
sequence rather than embedding resource orchestration in YAML.
"""

from __future__ import annotations

import argparse
import importlib.util
import os
import sys
from pathlib import Path
from types import ModuleType

BASELINE_JOBS = "1,2"
RAISED_JOBS = 8
SOURCE_DIR = Path(__file__).resolve().parent
TELEMETRY_SCRIPT = SOURCE_DIR / "cargo_orchestration_telemetry.py"


def load_telemetry_runner(path: Path = TELEMETRY_SCRIPT) -> ModuleType:
    """Load the sibling stdlib-only runner without making ``ci`` a package."""
    spec = importlib.util.spec_from_file_location("cargo_orchestration_telemetry", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"could not load telemetry runner: {path}")
    module = importlib.util.module_from_spec(spec)
    # Dataclass resolves its module while decorating Snapshot/ProcessCounts;
    # register this dynamically loaded sibling just as normal import does.
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def matrix_arguments(source_soldr: Path, evidence_root: Path) -> list[str]:
    """Construct the invariant N=1/N=2/N=8 source-Soldr check invocation."""
    return [
        "--jobs",
        BASELINE_JOBS,
        "--raised-jobs",
        str(RAISED_JOBS),
        "--allow-raised-count",
        "--case-root",
        str(evidence_root / "cases"),
        "--output",
        str(evidence_root / "telemetry.json"),
        "--",
        str(source_soldr),
        "cargo",
        "check",
        "-p",
        "soldr-cli",
    ]


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--source-soldr",
        type=Path,
        required=True,
        help="fresh source binary built from this checkout",
    )
    parser.add_argument(
        "--evidence-root",
        type=Path,
        required=True,
        help="new directory retained by the workflow artifact step",
    )
    parsed = parser.parse_args(argv)
    if not parsed.source_soldr.is_file() or not os.access(parsed.source_soldr, os.X_OK):
        parser.error(f"--source-soldr is not an executable file: {parsed.source_soldr}")
    if parsed.evidence_root.exists():
        parser.error("--evidence-root must not already exist; every dispatch is cold")
    return parsed


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    args.evidence_root.mkdir(parents=True)
    telemetry = load_telemetry_runner()
    return telemetry.main(matrix_arguments(args.source_soldr, args.evidence_root))


if __name__ == "__main__":
    raise SystemExit(main())
