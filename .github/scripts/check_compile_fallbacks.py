#!/usr/bin/env python3
"""Fail CI when a build silently degraded to the direct compiler (soldr#1838 Phase 4).

When a cacheable compile cannot reach the daemon, soldr runs the compiler
directly (uncached) and records it in `logs/compile-daemon-fallbacks.jsonl`.
Nothing in CI asserted on that -- a lane could quietly run uncached, which is
the "10-50x slower" symptom #1838 opens with, and the only trace was a stderr
line no step read.

This reads the `fallbacks` rollup that `soldr doctor --json` / `soldr status
--json` already expose (the same Rust rollup surfaced to operators) and exits
non-zero when any fallback was recorded, printing each reason so the log names
the cause. Pure JSON in, so the parsing is unit-tested without a daemon.

Usage:
    soldr doctor --json > doctor.json
    python3 .github/scripts/check_compile_fallbacks.py doctor.json
    # or pipe:  soldr doctor --json | python3 .github/scripts/check_compile_fallbacks.py -
Options:
    --allow N   tolerate up to N fallbacks before failing (default 0)
"""

from __future__ import annotations

import argparse
import json
import pathlib
import sys


def evaluate(doctor: dict, allow: int) -> "tuple[bool, int, list[str]]":
    """Return (ok, total, reason_lines) from a doctor/status JSON object.

    A missing or malformed `fallbacks` block is treated as zero — the guard
    must never fail a build because the *rollup* was absent, only because a
    genuine fallback was recorded. `ok` is True when total <= allow.
    """
    rollup = doctor.get("fallbacks")
    if not isinstance(rollup, dict):
        return True, 0, []
    total = rollup.get("total")
    if not isinstance(total, int):
        return True, 0, []
    recent = rollup.get("recent")
    reasons: list[str] = []
    if isinstance(recent, list):
        for entry in recent:
            if isinstance(entry, dict):
                reasons.append(str(entry.get("reason", "(reason unrecorded)")))
    return total <= allow, total, reasons


def _load(path: str) -> dict:
    text = (
        sys.stdin.read()
        if path == "-"
        else pathlib.Path(path).read_text(encoding="utf-8")
    )
    return json.loads(text)


def main(argv: "list[str] | None" = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "json_path",
        help="path to `soldr doctor --json` / `soldr status --json` output, or - for stdin",
    )
    parser.add_argument(
        "--allow",
        type=int,
        default=0,
        help="tolerate up to N fallbacks before failing (default 0)",
    )
    args = parser.parse_args(argv)

    try:
        doctor = _load(args.json_path)
    except (OSError, json.JSONDecodeError) as error:
        # An unreadable input is a wiring problem, not a build failure. Say so
        # and pass, so the guard cannot become a mysterious red on its own bug.
        print(f"check_compile_fallbacks: could not read {args.json_path}: {error}")
        return 0

    ok, total, reasons = evaluate(doctor, args.allow)
    if ok:
        print(
            f"check_compile_fallbacks: {total} compile-daemon fallback(s) (allowed {args.allow}) — OK"
        )
        return 0

    print(
        f"check_compile_fallbacks: {total} compile-daemon fallback(s) exceeded the "
        f"allowed {args.allow} — this lane silently ran UNCACHED via the direct "
        f"compiler (soldr#1838)."
    )
    for reason in reasons:
        print(f"  - {reason}")
    print(
        "  A fallback means the build did not reach the daemon. Investigate the "
        "daemon health on this lane; see docs/DAEMON_TIMEOUTS.md."
    )
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
