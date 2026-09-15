#!/usr/bin/env python3
"""Join soldr's shadow memory estimates to measured compiler peaks (soldr#3152).

WHY
    soldr#3152 replaces admission name lists with a per-unit peak-memory
    estimate spent against live headroom. An estimate is only safe to spend
    once it has been checked against reality: an under-estimate is an OOM.
    This script is that check. It is the offline half of shadow mode.

WHAT IT READS
    * `admission-estimate.jsonl`: one row per Rust compiler child soldr's
      daemon classified, written by
      `crates/soldr-daemon/src/memory_estimate.rs::record`. Each row carries
      `estimate_bytes`, the features it was derived from, the admission
      decision (`exclusive`), and `args_digest`.
    * zccache `compile_journal.jsonl` (and rotated siblings / history
      copies): each compile's full `args` and, from zccache 1.13.23,
      `child_peak_rss_bytes`, the largest resident high-water mark sampled
      from the compiler children that request spawned.

JOIN KEY
    `args_digest` = lower-case hex SHA-256 of every argument followed by one
    NUL byte. The daemon computes it at admission; this script recomputes it
    from the journal's `args`. Both sides pin the same test vector, so they
    cannot drift silently.

WHAT IT REPORTS
    * `joined`: estimates matched to a journal row that carries a measured
      peak. When several journal rows share a digest, the largest peak wins,
      because that is the one admission must not under-predict.
    * `unmeasured`: matched, but no journal row for the digest has a peak (a
      cache hit, an older zccache, or a platform that could not measure).
    * `unjoined`: no journal row at all for the digest.
    * `under_predicted` and `worst_under_predictions`: rows whose measured
      peak exceeds the estimate, ranked by measured/estimate.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
from pathlib import Path
from typing import Any, Iterable

JOURNAL_NAME = "compile_journal.jsonl"


def args_digest(args: Iterable[str]) -> str:
    """Hex SHA-256 of each argument followed by a NUL byte."""
    hasher = hashlib.sha256()
    for arg in args:
        hasher.update(arg.encode("utf-8"))
        hasher.update(b"\0")
    return hasher.hexdigest()


def _read_jsonl(path: Path) -> Iterable[dict[str, Any]]:
    with path.open(encoding="utf-8", errors="replace") as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            try:
                row = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(row, dict):
                yield row


def discover_journals(paths: Iterable[Path]) -> list[Path]:
    """Journal files named directly, or found under directories."""
    found: set[Path] = set()
    for path in paths:
        if path.is_file():
            found.add(path)
        elif path.is_dir():
            for candidate in path.rglob(f"{JOURNAL_NAME}*"):
                if candidate.is_file():
                    found.add(candidate)
    return sorted(found)


def measured_peaks(journals: Iterable[Path]) -> dict[str, int | None]:
    """Largest measured peak per args digest; None when never measured."""
    peaks: dict[str, int | None] = {}
    for journal in journals:
        for row in _read_jsonl(journal):
            args = row.get("args")
            if not isinstance(args, list) or not all(isinstance(a, str) for a in args):
                continue
            digest = args_digest(args)
            peak = row.get("child_peak_rss_bytes")
            current = peaks.get(digest)
            if isinstance(peak, int) and peak > 0:
                peaks[digest] = peak if current is None else max(current, peak)
            else:
                peaks.setdefault(digest, None)
    return peaks


def analyze(
    estimate_logs: Iterable[Path], journal_paths: Iterable[Path], top: int = 10
) -> dict[str, Any]:
    """Join estimates to measured peaks and summarise prediction error."""
    peaks = measured_peaks(discover_journals(journal_paths))
    estimates = 0
    joined = 0
    unmeasured = 0
    unjoined = 0
    pairs: list[dict[str, Any]] = []
    for log in estimate_logs:
        for row in _read_jsonl(log):
            estimate = row.get("estimate_bytes")
            digest = row.get("args_digest")
            if not isinstance(estimate, int) or not isinstance(digest, str):
                continue
            estimates += 1
            if digest not in peaks:
                unjoined += 1
                continue
            measured = peaks[digest]
            if measured is None:
                unmeasured += 1
                continue
            joined += 1
            pairs.append(
                {
                    "crate_name": row.get("crate_name"),
                    "is_test": row.get("is_test"),
                    "lto": row.get("lto"),
                    "exclusive": row.get("exclusive"),
                    "estimate_bytes": estimate,
                    "measured_bytes": measured,
                    "measured_over_estimate": measured / estimate if estimate else None,
                }
            )

    under = [p for p in pairs if p["measured_bytes"] > p["estimate_bytes"]]
    under.sort(key=lambda p: p["measured_over_estimate"] or 0.0, reverse=True)
    ratios = [p["measured_over_estimate"] for p in pairs if p["measured_over_estimate"]]
    return {
        "estimates": estimates,
        "joined": joined,
        "unmeasured": unmeasured,
        "unjoined": unjoined,
        "under_predicted": len(under),
        "max_measured_over_estimate": max(ratios) if ratios else None,
        "worst_under_predictions": under[:top],
    }


def _format_text(report: dict[str, Any]) -> str:
    lines = [
        f"estimates: {report['estimates']}",
        f"joined (measured peak): {report['joined']}",
        f"unmeasured (no peak on any matching journal row): {report['unmeasured']}",
        f"unjoined (no matching journal row): {report['unjoined']}",
        f"under-predicted: {report['under_predicted']}",
        f"max measured/estimate: {report['max_measured_over_estimate']}",
    ]
    for pair in report["worst_under_predictions"]:
        lines.append(
            f"  {pair['crate_name']}: measured {pair['measured_bytes']} > "
            f"estimate {pair['estimate_bytes']} "
            f"(x{pair['measured_over_estimate']:.2f})"
        )
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--estimates",
        nargs="+",
        required=True,
        type=Path,
        help="admission-estimate.jsonl file(s) written by soldr-daemon",
    )
    parser.add_argument(
        "--journal",
        nargs="+",
        required=True,
        type=Path,
        help="compile_journal.jsonl file(s) or directories to search",
    )
    parser.add_argument("--top", type=int, default=10)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args(argv)
    report = analyze(args.estimates, args.journal, top=args.top)
    if args.json:
        print(json.dumps(report, indent=2))
    else:
        print(_format_text(report))
    return 0


if __name__ == "__main__":
    sys.exit(main())
