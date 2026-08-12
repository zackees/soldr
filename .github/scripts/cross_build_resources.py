#!/usr/bin/env python3
"""Select bounded compile concurrency for the cross-build archive stage.

The x86_64 Linux nextest archives are the largest all-miss link sets in the
per-target builder fleet. Two concurrent compiler processes exceeded the
hosted runner's memory ceiling in soldr#2481 even with enlarged swap, so those
archives are serialized. Other targets retain the established two-job bound.
"""

from __future__ import annotations

import argparse
import os
from pathlib import Path

DEFAULT_ARCHIVE_JOBS = 2
ARCHIVE_JOB_OVERRIDES = {
    "x86_64-unknown-linux-gnu": 1,
    "x86_64-unknown-linux-musl": 1,
}


def archive_jobs(target: str) -> int:
    """Return the compile/admission limit for *target*'s nextest archive."""

    return ARCHIVE_JOB_OVERRIDES.get(target, DEFAULT_ARCHIVE_JOBS)


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
