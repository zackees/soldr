#!/usr/bin/env python3
"""Require the selected CI tier on one candidate before a PR may merge."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import runpy
from pathlib import Path
from typing import Any, TypedDict

COVERAGE = runpy.run_path(str(Path(__file__).with_name("ci_full_coverage.py")))
SELECTOR = runpy.run_path(str(Path(__file__).with_name("ci_mode.py")))
SCHEMA = "fleet-ci-summary/v1"


class Selection(TypedDict):
    """The resolved mode and identity supplied by the workflow."""

    mode: str
    docs_only: bool
    event_name: str
    author_permission: str
    expected_sha: str
    selected_sha: str


def required_jobs(
    adapter: dict[str, Any], full: dict[str, Any], mode: str, docs_only: bool
) -> set[str]:
    """Adapters declare a meaningful minimal gate and nonempty test expansion."""
    groups = {}
    for name in ("minimal_jobs", "docs_jobs", "test_jobs"):
        groups[name] = COVERAGE["required_jobs"](
            {
                "schema_version": adapter.get("schema_version"),
                "required_jobs": adapter.get(name),
                "targets": [],
            }
        )
    policy_jobs = {"ci-mode", "path-selection", "full-coverage"}
    if any(group & policy_jobs for group in groups.values()):
        raise ValueError("tier jobs cannot name policy prerequisites")
    if not groups["test_jobs"] - groups["minimal_jobs"]:
        raise ValueError("test tier must add jobs beyond minimal")
    jobs = {"ci-mode", "path-selection"}
    if mode == "minimal":
        return jobs | groups["docs_jobs" if docs_only else "minimal_jobs"]
    if mode == "test":
        return jobs | groups["minimal_jobs"] | groups["test_jobs"]
    if mode == "full":
        return (
            jobs
            | groups["minimal_jobs"]
            | groups["test_jobs"]
            | COVERAGE["required_jobs"](full)
            | {"full-coverage"}
        )
    raise ValueError("missing or unknown CI mode")


def summary_failures(
    adapter: dict[str, Any],
    full: dict[str, Any],
    needs: dict[str, Any],
    *,
    mode: str,
    docs_only: bool,
    event_name: str,
    author_permission: str,
    expected_sha: str,
    selected_sha: str,
) -> list[str]:
    failures = []
    if event_name not in ("pull_request", "push", "workflow_dispatch"):
        failures.append("missing or unsupported event")
    if (
        event_name == "pull_request"
        and author_permission not in ("write", "maintain", "admin")
        and mode != "full"
    ):
        failures.append("external or unresolved author requires full CI")
    if event_name == "workflow_dispatch" and mode != "full":
        failures.append("candidate dispatch requires full CI")
    jobs = required_jobs(adapter, full, mode, docs_only)
    failures += COVERAGE["coverage_failures"](
        {"schema_version": 1, "required_jobs": sorted(jobs), "targets": []},
        needs,
        expected_sha=expected_sha,
        selected_sha=selected_sha,
    )
    if mode == "full":
        # Retain the canonical refusal of build-only/blocked execution cells.
        failures += COVERAGE["coverage_failures"](
            full, needs, expected_sha=expected_sha, selected_sha=selected_sha
        )
    return sorted(set(failures))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--adapter", type=Path, required=True)
    parser.add_argument("--full-contract", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    adapter_raw = args.adapter.read_bytes()
    full_raw = args.full_contract.read_bytes()
    adapter, full = json.loads(adapter_raw), json.loads(full_raw)
    needs = json.loads(os.environ["CI_NEEDS_JSON"])
    inputs: Selection = {
        "mode": os.environ.get("CI_MODE", ""),
        "docs_only": os.environ.get("DOCS_ONLY", "") == "true",
        "event_name": os.environ.get("GITHUB_EVENT_NAME", ""),
        "author_permission": os.environ.get("AUTHOR_PERMISSION", "unknown"),
        "expected_sha": os.environ.get("EXPECTED_SHA", ""),
        "selected_sha": os.environ.get("SELECTED_SHA", ""),
    }
    selector_permission = inputs["author_permission"]
    if inputs["event_name"] == "pull_request":
        # A failed-job rerun can reuse ci-mode's outputs from an earlier
        # attempt. The merge decision must use permission at summary time.
        try:
            event = json.loads(
                Path(os.environ.get("GITHUB_EVENT_PATH", "")).read_text(
                    encoding="utf-8"
                )
            )
            permission = (
                SELECTOR["lookup_author_permission"](
                    event,
                    os.environ.get("GITHUB_REPOSITORY", ""),
                    os.environ.get("GITHUB_TOKEN", ""),
                )
                if isinstance(event, dict)
                else None
            )
        except (OSError, ValueError):
            permission = None
        inputs["author_permission"] = permission or "unknown"
    try:
        failures = summary_failures(adapter, full, needs, **inputs)
        jobs = sorted(required_jobs(adapter, full, inputs["mode"], inputs["docs_only"]))
    except (ValueError, TypeError, KeyError) as error:
        failures, jobs = [f"invalid CI contract or selection: {error}"], []
    report = {
        "schema": SCHEMA,
        **inputs,
        "selector_author_permission": selector_permission,
        "adapter_sha256": hashlib.sha256(adapter_raw).hexdigest(),
        "manifest_sha256": hashlib.sha256(full_raw).hexdigest(),
        "required_jobs": jobs,
        "outcomes": {
            job: (
                needs[job].get("result", "missing")
                if isinstance(needs.get(job), dict)
                else "missing"
            )
            for job in jobs
        },
        "failures": failures,
        "success": not failures,
    }
    args.report.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    for failure in failures:
        print(f"::error::CI summary failed: {failure}")
    return int(bool(failures))


if __name__ == "__main__":
    raise SystemExit(main())
