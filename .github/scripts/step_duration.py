#!/usr/bin/env python3
"""Run a command, time it, and record the duration in the job step summary.

Replaces the PowerShell stopwatch boilerplate that `_build-and-test.yml`
repeated in every timed step. The workflow keeps a one-line bash invocation and
the logic lives here, where it can be unit-tested and run from a shell without
pushing a branch (CLAUDE.md, "GitHub Actions workflow conventions").

Usage:
    python3 .github/scripts/step_duration.py --label "target / Build" -- cmd args...

The command's exit code is this script's exit code, so a failing step still
fails the lane. The summary line is written even on failure -- a slow step that
then failed is exactly when the timing is worth having.
"""

from __future__ import annotations

import argparse
import os
import pathlib
import subprocess
import sys
import time


def append_summary(label: str, seconds: float) -> None:
    """Append one bullet to $GITHUB_STEP_SUMMARY, if the runner provided it."""
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if not summary:
        return
    line = f"- **{label}**: {seconds:.3f}s\n"
    try:
        with pathlib.Path(summary).open("a", encoding="utf-8") as handle:
            handle.write(line)
    except OSError as error:
        # Never fail a lane because the summary file was unwritable.
        print(f"step_duration: could not write step summary: {error}", file=sys.stderr)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--label", required=True, help="step summary label")
    parser.add_argument(
        "command",
        nargs=argparse.REMAINDER,
        help="command to run, after a bare `--`",
    )
    args = parser.parse_args(argv)

    command = args.command
    if command and command[0] == "--":
        command = command[1:]
    if not command:
        parser.error("no command given; pass it after `--`")

    started = time.monotonic()
    completed = subprocess.run(command, check=False)
    elapsed = time.monotonic() - started

    append_summary(args.label, elapsed)
    print(
        f"step_duration: {args.label} took {elapsed:.3f}s (exit {completed.returncode})"
    )
    return completed.returncode


if __name__ == "__main__":
    sys.exit(main())
