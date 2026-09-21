#!/usr/bin/env python3
"""Select PR-only CI jobs while keeping ci.yml as the sole PR entry point."""

from __future__ import annotations

import argparse
import fnmatch
import json
import os
import subprocess
from pathlib import Path, PurePosixPath

POLICIES = {
    "run_setup_soldr": (
        "action.yml",
        "install.sh",
        "rust-toolchain.toml",
        "Cargo.toml",
        "Cargo.lock",
        ".github/actions/setup-soldr/**",
        ".github/workflows/setup-soldr-action.yml",
    ),
    "run_cook_size_gate": (
        "crates/soldr-cli/src/cook.rs",
        "crates/soldr-cache/src/cache_lib/strip_target.rs",
        "crates/soldr-cache/**",
        ".github/workflows/cook-size-gate.yml",
        "tests/test_cook_size_gate_workflow.py",
    ),
}


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


def matches(path: str, patterns: tuple[str, ...]) -> bool:
    return any(fnmatch.fnmatchcase(normalized(path), pattern) for pattern in patterns)


def select(event_name: str, changed_paths: list[str]) -> dict[str, str]:
    """Return stable string outputs for job-level GitHub expressions."""

    if event_name != "pull_request" or not changed_paths:
        return {"docs_only": "false", **dict.fromkeys(POLICIES, "false")}
    return {
        "docs_only": str(
            all(is_docs_only_path(path) for path in changed_paths)
        ).lower(),
        **{
            name: str(any(matches(path, patterns) for path in changed_paths)).lower()
            for name, patterns in POLICIES.items()
        },
    }


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
