#!/usr/bin/env python3
"""Fail a full CI run when any canonical target or smoke job was skipped."""

from __future__ import annotations

import json
import os
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
EXTRA_REQUIRED = {
    "lint",
    "build-linux-x64",
    "pep517-daemon-smoke",
    "windows-e2e-policy",
    "e2e-cross-bootstrap-soldr",
    "e2e-linux-x64-gnu-build",
    "wheel-cross-policy",
    "wheel-cross-verify",
}


def required_jobs(contract: dict[str, object]) -> set[str]:
    jobs = set(EXTRA_REQUIRED)
    for target in contract["targets"]:
        ci = target["ci"]
        jobs.add(ci["build_job"])
        if ci.get("run_job"):
            jobs.add(ci["run_job"])
    return jobs


def coverage_failures(
    contract: dict[str, object], needs: dict[str, object]
) -> list[str]:
    failures = []
    for job in sorted(required_jobs(contract)):
        result = needs.get(job)
        state = result.get("result") if isinstance(result, dict) else "missing"
        if state != "success":
            failures.append(f"{job}: {state or 'missing'}")
    for target in contract["targets"]:
        ci = target["ci"]
        if ci.get("run_job"):
            continue
        issue = ci.get("execution_exception", {}).get("issue")
        reference = f" (soldr#{issue})" if issue else ""
        failures.append(f"{target['triple']}: no full-CI execution job{reference}")
    return failures


def main() -> int:
    contract = json.loads((ROOT / "ci" / "canonical-targets.json").read_text())
    needs = json.loads(os.environ["CI_NEEDS_JSON"])
    failures = coverage_failures(contract, needs)
    if failures:
        for failure in failures:
            print(f"::error::full CI coverage incomplete: {failure}")
        return 1
    print("Full CI coverage complete for all canonical CI targets and smoke jobs")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
