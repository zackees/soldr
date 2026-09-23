#!/usr/bin/env python3
"""Require successful full CI for the exact release candidate before building."""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import urllib.request


class GateError(RuntimeError):
    """Release candidate has no acceptable full-CI proof."""


def validate(run: dict, jobs: list[dict], *, sha: str, repository: str) -> None:
    if not re.fullmatch(r"[0-9a-f]{40}", sha):
        raise GateError("candidate_sha must be a full lowercase commit SHA")
    workflow_path = str(run.get("path", "")).split("@", 1)[0]
    if (
        run.get("event") != "workflow_dispatch"
        or workflow_path != ".github/workflows/ci.yml"
    ):
        raise GateError("proof must be an explicit CI workflow_dispatch run")
    if run.get("repository", {}).get("full_name") != repository:
        raise GateError("CI run belongs to another repository")
    if run.get("head_branch") != "main":
        raise GateError("CI proof must run from the main workflow branch")
    # Dispatch runs can be started from main after its tip has advanced. The
    # CI run-name records candidate_sha, and ci-mode verifies its checkout.
    if run.get("display_title") != f"CI full {sha}":
        raise GateError("CI dispatch candidate SHA does not match release candidate")
    if run.get("status") != "completed" or run.get("conclusion") != "success":
        raise GateError("full CI run has not completed successfully")
    for name in ("CI mode", "Full coverage"):
        matches = [job for job in jobs if job.get("name") == name]
        if (
            len(matches) != 1
            or matches[0].get("status") != "completed"
            or matches[0].get("conclusion") != "success"
        ):
            raise GateError(
                f"required CI job {name} is missing, skipped, or unsuccessful"
            )


def get_json(url: str, token: str) -> dict:
    request = urllib.request.Request(
        url,
        headers={
            "Authorization": f"Bearer {token}",
            "Accept": "application/vnd.github+json",
        },
    )
    with urllib.request.urlopen(request, timeout=20) as response:
        return json.load(response)


def verify_candidate(sha: str) -> None:
    if not re.fullmatch(r"[0-9a-f]{40}", sha):
        raise GateError("candidate_sha must be a full lowercase commit SHA")
    head = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
    if head != sha:
        raise GateError("checked-out candidate does not match candidate_sha")
    subprocess.run(["git", "fetch", "origin", "main"], check=True)
    if subprocess.run(
        ["git", "merge-base", "--is-ancestor", sha, "origin/main"], check=False
    ).returncode:
        raise GateError("candidate is not a merged commit reachable from main")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--candidate-sha", required=True)
    parser.add_argument("--ci-run-id", type=int)
    parser.add_argument("--repository")
    parser.add_argument("--verify-candidate", action="store_true")
    args = parser.parse_args()
    if args.verify_candidate:
        verify_candidate(args.candidate_sha)
        return 0
    if not args.repository or args.ci_run_id is None:
        raise GateError("ci_run_id and repository are required for CI proof")
    if not re.fullmatch(r"[0-9a-f]{40}", args.candidate_sha):
        raise GateError("candidate_sha must be a full lowercase commit SHA")
    if args.ci_run_id <= 0:
        raise GateError("ci_run_id must be positive")
    token = os.environ["GH_TOKEN"]
    base = (
        f"https://api.github.com/repos/{args.repository}/actions/runs/{args.ci_run_id}"
    )
    run = get_json(base, token)
    jobs = []
    page = 1
    while True:
        response = get_json(
            f"{base}/jobs?filter=latest&per_page=100&page={page}", token
        )
        batch = response["jobs"]
        jobs.extend(batch)
        if len(batch) < 100:
            break
        page += 1
    validate(run, jobs, sha=args.candidate_sha, repository=args.repository)
    print(f"Full coverage verified for {args.candidate_sha}: {run['html_url']}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (GateError, KeyError, OSError, ValueError) as exc:
        raise SystemExit(f"release full-CI gate failed: {exc}") from exc
