#!/usr/bin/env python3
"""Serialize the memory-heavy cross-build nextest archive stage.

The all-miss archive link set has exceeded the hosted runner's memory ceiling
on x86_64 Linux, aarch64 Linux, and aarch64 Windows even with enlarged swap.
Every target therefore uses one Cargo producer and one Soldr admission slot.
"""

from __future__ import annotations

import argparse
import os
from pathlib import Path

ARCHIVE_JOBS = 1


def archive_jobs(target: str) -> int:
    """Return the compile/admission limit for *target*'s nextest archive."""

    del target
    return ARCHIVE_JOBS


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True)
    parser.add_argument("--github-env", default=os.environ.get("GITHUB_ENV", ""))
    args = parser.parse_args()

    if not args.github_env:
        parser.error("--github-env (or GITHUB_ENV) is required")
    jobs = archive_jobs(args.target)
    with Path(args.github_env).open("a", encoding="utf-8") as github_env:
        github_env.write(f"CARGO_BUILD_JOBS={jobs}\n")
        github_env.write(f"SOLDR_JOBS={jobs}\n")
    print(f"nextest archive resources: target={args.target} jobs={jobs}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
