#!/usr/bin/env python3
"""Select routine or complete CI coverage and pin the source commit."""

from __future__ import annotations

import argparse
import json
import os
import re
from pathlib import Path


def select_mode(
    event_name: str, event: dict[str, object], candidate_sha: str
) -> tuple[str, str]:
    if event_name == "workflow_dispatch":
        if not re.fullmatch(r"[0-9a-fA-F]{40}", candidate_sha):
            raise ValueError(
                "workflow_dispatch requires a full 40-character candidate SHA"
            )
        return "full", candidate_sha.lower()
    if event_name == "pull_request":
        pr = event.get("pull_request")
        if not isinstance(pr, dict):
            raise ValueError("pull_request payload is missing")
        head = pr.get("head")
        sha = head.get("sha") if isinstance(head, dict) else None
        if not isinstance(sha, str) or not re.fullmatch(r"[0-9a-fA-F]{40}", sha):
            raise ValueError("pull_request head SHA is missing or invalid")
        labels = pr.get("labels")
        names = (
            {label.get("name") for label in labels if isinstance(label, dict)}
            if isinstance(labels, list)
            else set()
        )
        mode = (
            "full"
            if "ci-full" in names
            else "test" if "ci-test" in names else "minimal"
        )
        return mode, sha.lower()
    if event_name == "push":
        sha = event.get("after")
        if not isinstance(sha, str) or not re.fullmatch(r"[0-9a-fA-F]{40}", sha):
            raise ValueError("push commit SHA is missing or invalid")
        return "minimal", sha.lower()
    raise ValueError(f"unsupported CI event: {event_name}")


def verify_checkout(expected_sha: str, checked_out_sha: str) -> None:
    if checked_out_sha.lower() != expected_sha:
        raise ValueError(f"checked out {checked_out_sha}, expected {expected_sha}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--event-name", default=os.environ.get("GITHUB_EVENT_NAME", ""))
    parser.add_argument("--event-path", default=os.environ.get("GITHUB_EVENT_PATH", ""))
    parser.add_argument("--candidate-sha", default="")
    parser.add_argument("--checked-out-sha", required=True)
    parser.add_argument("--github-output", default=os.environ.get("GITHUB_OUTPUT", ""))
    args = parser.parse_args()
    event = json.loads(Path(args.event_path).read_text(encoding="utf-8"))
    mode, sha = select_mode(args.event_name, event, args.candidate_sha)
    verify_checkout(sha, args.checked_out_sha)
    if not args.github_output:
        raise ValueError("GITHUB_OUTPUT is required")
    with Path(args.github_output).open("a", encoding="utf-8") as output:
        output.write(f"mode={mode}\ncheckout_sha={sha}\n")
    print(f"CI mode: {mode}; checked out SHA: {sha}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
