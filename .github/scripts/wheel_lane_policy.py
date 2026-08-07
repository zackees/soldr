#!/usr/bin/env python3
"""Decide whether the `soldr wheel` cross-verification lane runs (soldr#2139).

The lane it gates is expensive: an ubuntu-24.04 runner that cross-builds a
release wheel for `aarch64-unknown-linux-gnu` through the blessed toolchain,
then unpacks it and reads the embedded binary's glibc requirements. soldr#1978
is an ongoing effort to cut CI spend, so this must not run on every PR.

Policy:

* every non-`pull_request` event (push to main, workflow_dispatch, schedule)
  runs it -- main is the branch a release is cut from, so the acceptance
  criterion is checked before it can matter;
* a pull request runs it only when it touches a path that can change what the
  wheel is or how it is tagged;
* an unclassifiable pull request (empty diff) fails *open* and runs, because a
  gate that silently stops gating is worse than one runner-minute.

Output is a matrix rather than a boolean so the caller can select the job's
matrix via `fromJSON`. An empty matrix means the job is never scheduled and
never allocates a runner, which a step-level `if:` would not achieve.

Usage:
    python3 .github/scripts/wheel_lane_policy.py
    python3 .github/scripts/wheel_lane_policy.py --github-output /dev/stdout
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
from pathlib import Path, PurePosixPath
from typing import NamedTuple, Sequence

# The one cell the lane runs. Kept as data so adding a second target later is
# a list edit rather than a workflow rewrite.
WHEEL_MATRIX = [
    {
        "name": "linux-arm64",
        "runner": "ubuntu-24.04",
        "target": "aarch64-unknown-linux-gnu",
        "expected_tag": "manylinux_2_17",
        "max_glibc": "2.17",
    }
]

# Exact files that decide what the wheel contains or claims.
WHEEL_FILES = frozenset(
    {
        "crates/soldr-cli/src/wheel_cmd.rs",
        "pyproject.toml",
        ".github/workflows/ci.yml",
        ".github/scripts/wheel_lane_policy.py",
        ".github/scripts/verify_wheel_glibc.py",
        ".github/scripts/verify_glibc_baseline.py",
    }
)


class Decision(NamedTuple):
    run: bool
    reason: str


def _normalized(path: str) -> str:
    normalized = str(PurePosixPath(path.replace("\\", "/")))
    return normalized[2:] if normalized.startswith("./") else normalized


def is_wheel_relevant_path(path: str) -> bool:
    """Whether *path* can change the wheel's contents or its platform tag."""

    normalized = _normalized(path)
    return normalized in WHEEL_FILES


def decide_wheel_lane(
    *, event_name: str, changed_paths: Sequence[str]
) -> Decision:
    if event_name != "pull_request":
        return Decision(True, f"{event_name or 'non-PR'} event: always verify")
    if not changed_paths:
        return Decision(True, "empty diff cannot be classified; failing open")
    relevant = [path for path in changed_paths if is_wheel_relevant_path(path)]
    if relevant:
        return Decision(True, f"wheel-relevant path changed: {relevant[0]}")
    return Decision(False, "no wheel-relevant path changed")


def _pull_request_paths(event: "dict[str, object]") -> "list[str]":
    pull_request = event.get("pull_request")
    if not isinstance(pull_request, dict):
        return []
    base = pull_request.get("base")
    head = pull_request.get("head")
    if not isinstance(base, dict) or not isinstance(head, dict):
        return []
    base_sha = base.get("sha")
    head_sha = head.get("sha")
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
    return [line for line in result.stdout.splitlines() if line]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--event-name", default=os.environ.get("GITHUB_EVENT_NAME", ""))
    parser.add_argument("--event-path", default=os.environ.get("GITHUB_EVENT_PATH", ""))
    parser.add_argument("--github-output", default=os.environ.get("GITHUB_OUTPUT", ""))
    args = parser.parse_args()

    event: "dict[str, object]" = {}
    if args.event_path and Path(args.event_path).is_file():
        event = json.loads(Path(args.event_path).read_text(encoding="utf-8"))
    paths = _pull_request_paths(event) if args.event_name == "pull_request" else []
    decision = decide_wheel_lane(event_name=args.event_name, changed_paths=paths)
    matrix = WHEEL_MATRIX if decision.run else []

    print(
        f"soldr wheel lane: {'run' if decision.run else 'skip'} ({decision.reason})"
    )
    if not args.github_output:
        raise SystemExit("--github-output (or GITHUB_OUTPUT) is required")
    with Path(args.github_output).open("a", encoding="utf-8") as output:
        output.write(f"matrix={json.dumps(matrix)}\n")
        output.write(f"run_wheel_lane={'true' if decision.run else 'false'}\n")
        output.write(f"reason={decision.reason}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
