#!/usr/bin/env python3
"""Identify documentation-only PRs for the cheap routine lint status."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
from pathlib import Path, PurePosixPath


def normalized(path: str) -> str:
    path = str(PurePosixPath(path.replace("\\", "/")))
    return path[2:] if path.startswith("./") else path


def is_docs_only_path(path: str) -> bool:
    path = normalized(path).lower()
    name = PurePosixPath(path).name
    return (
        path.endswith(".md")
        or path.startswith("docs/")
        or path.startswith(".github/issue_template/")
        or path.startswith(".github/pull_request_template")
        or name.startswith("license")
    )


def select(event_name: str, changed_paths: list[str]) -> dict[str, str]:
    """Return stable string outputs for job-level GitHub expressions."""

    docs_only = (
        event_name == "pull_request"
        and bool(changed_paths)
        and all(is_docs_only_path(path) for path in changed_paths)
    )
    return {"docs_only": str(docs_only).lower()}


def pull_request_paths(event: dict[str, object]) -> list[str]:
    pr = event.get("pull_request")
    if not isinstance(pr, dict):
        return []
    base, head = pr.get("base"), pr.get("head")
    if not isinstance(base, dict) or not isinstance(head, dict):
        return []
    base_sha, head_sha = base.get("sha"), head.get("sha")
    if not isinstance(base_sha, str) or not isinstance(head_sha, str):
        return []
    result = subprocess.run(
        [
            "git",
            "diff",
            "--name-only",
            "--diff-filter=ACDMRTUXB",
            f"{base_sha}...{head_sha}",
        ],
        check=True,
        capture_output=True,
        text=True,
    )
    return [path for path in result.stdout.splitlines() if path]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--event-name", default=os.environ.get("GITHUB_EVENT_NAME", ""))
    parser.add_argument("--event-path", default=os.environ.get("GITHUB_EVENT_PATH", ""))
    parser.add_argument("--github-output", default=os.environ.get("GITHUB_OUTPUT", ""))
    args = parser.parse_args()
    event = (
        json.loads(Path(args.event_path).read_text(encoding="utf-8"))
        if args.event_path
        else {}
    )
    outputs = select(args.event_name, pull_request_paths(event))
    if not args.github_output:
        raise SystemExit("--github-output (or GITHUB_OUTPUT) is required")
    with Path(args.github_output).open("a", encoding="utf-8") as destination:
        for key, value in outputs.items():
            destination.write(f"{key}={value}\n")
    print(
        "CI path selection: "
        + ", ".join(f"{key}={value}" for key, value in outputs.items())
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
