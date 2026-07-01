#!/usr/bin/env python3
"""Skip perf / benchmark workflows if a real user push landed on main recently.

The perf-matrix and benchmark-stats workflows are expensive (build + bench +
publish, ~30-60 min a piece). Running them on every push to main during
active development bursts eats CI capacity that PR pipelines could use, and
produces no useful signal — the per-commit noise dominates the between-day
trend the benchmarks are meant to track.

This gate looks at recent main commits and skips the run when the most
recent NON-BOT commit is younger than `--hours` (default 24). The idea is
that perf-heavy workflows fire only during quiet periods — one benchmark
run per day of quiet — while active dev bursts stay lean.

The signal is deliberately "commit author is a human", not "the current
push is from a human" — if a bot pushed 20 minutes ago but the last actual
person-driven change was 30h ago, we still want the perf run.

## Bot classifiers

An author email counts as bot-authored when it matches any of:
    * `github-actions[bot]@...` — the default GHA identity for
      `${{ secrets.GITHUB_TOKEN }}` pushes.
    * `actions@github.com` — legacy GHA push identity.
    * `noreply@github.com` — used by some Squash/Merge web flows and by
      programmatic pushes via the GitHub API.
    * `renovate[bot]` / `dependabot[bot]` — dependency-update bots.
    * `soldr-release-bot` — hypothetical future release automation
      identity.

`noreply@anthropic.com` (Claude's co-author trailer) is NOT classified as
bot: it appears only in the co-author trailer of commits whose primary
author is a real user, and `git log --format=%ae` reports the AUTHOR
email, not co-author trailers.

## Usage

    python3 .github/scripts/perf_gate.py --hours 24

Writes `should_run=<true|false>` and `reason=<text>` to `$GITHUB_OUTPUT`
when running under GHA. Also prints the reason to stdout so `gh run view`
lines up with the gate decision. Always exits 0.

## Bypass rules baked in the caller

This script is only invoked when the workflow's own YAML has decided the
event might be gate-eligible (e.g. `push` to `main`). Workflow_dispatch
runs, `perf/**` branch pushes, and `evaluate/**` branch pushes should
bypass the gate entirely — those are explicit human requests to run.
Callers wire that bypass in with a plain shell check before invoking the
script; the script assumes it's being called in a gate-eligible context.
"""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
import time
from typing import Iterable

BOT_EMAIL_PATTERNS: tuple[re.Pattern[str], ...] = (
    re.compile(r"^github-actions\[bot\]@", re.IGNORECASE),
    re.compile(r"^actions@github\.com$", re.IGNORECASE),
    re.compile(r"^noreply@github\.com$", re.IGNORECASE),
    re.compile(r"renovate\[bot\]", re.IGNORECASE),
    re.compile(r"dependabot\[bot\]", re.IGNORECASE),
    re.compile(r"soldr-release-bot", re.IGNORECASE),
)


def is_bot(email: str) -> bool:
    email = email.strip()
    return any(p.search(email) for p in BOT_EMAIL_PATTERNS)


def walk_main_commits(limit: int, ref: str) -> Iterable[tuple[int, str]]:
    """Yield `(unix_ts, author_email)` for the most recent `limit` commits on `ref`."""
    proc = subprocess.run(
        ["git", "log", f"-n{limit}", "--format=%at|%ae", ref],
        capture_output=True,
        text=True,
        check=True,
    )
    for line in proc.stdout.splitlines():
        ts_str, sep, email = line.partition("|")
        if not sep:
            continue
        try:
            yield int(ts_str), email
        except ValueError:
            continue


def emit_output(should_run: bool, reason: str) -> None:
    print(reason)
    out_path = os.environ.get("GITHUB_OUTPUT")
    if not out_path:
        return
    with open(out_path, "a", encoding="utf-8") as fh:
        fh.write(f"should_run={'true' if should_run else 'false'}\n")
        # Escape newlines defensively; the reason is a single line by construction.
        fh.write(f"reason={reason}\n")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n", 1)[0])
    ap.add_argument(
        "--hours",
        type=int,
        default=24,
        help="Skip if the last non-bot commit is younger than this many hours.",
    )
    ap.add_argument(
        "--limit",
        type=int,
        default=200,
        help="Number of recent commits to walk (bounds the git log call).",
    )
    ap.add_argument(
        "--ref",
        default="HEAD",
        help="Git ref to walk. Defaults to HEAD (which is `main` when the "
        "workflow triggered on push:main).",
    )
    args = ap.parse_args()

    now = int(time.time())
    cutoff_ts = now - args.hours * 3600

    latest_human_ts: int | None = None
    latest_human_email: str | None = None
    for ts, email in walk_main_commits(args.limit, args.ref):
        if is_bot(email):
            continue
        latest_human_ts = ts
        latest_human_email = email
        break

    if latest_human_ts is None:
        # No non-bot commit in the walk window. Extremely unlikely in
        # practice; if it happens, treat as "quiet enough to run" so we
        # don't silently kill perf runs when the bot-detection heuristic
        # accidentally overmatches.
        emit_output(
            True,
            f"No non-bot commit in the last {args.limit} main commits — proceeding.",
        )
        return 0

    hours_ago = (now - latest_human_ts) // 3600
    if latest_human_ts >= cutoff_ts:
        emit_output(
            False,
            f"Last non-bot commit was {hours_ago}h ago ({latest_human_email}); "
            f"< {args.hours}h. Skipping perf run to preserve CI capacity for "
            "active dev.",
        )
        return 0

    emit_output(
        True,
        f"Last non-bot commit was {hours_ago}h ago ({latest_human_email}); "
        f">= {args.hours}h. Proceeding with perf run.",
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
