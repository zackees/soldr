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
    `unit_key` = `<crate name>/<cargo -C metadata>`. zccache hands admission
    the argument vector it executes, which it may have rewritten, but journals
    the client's original arguments, so a hash of the whole command line does
    not survive: the first CI data joined 92 of 1,235 estimates that way.
    Cargo's per-unit metadata hash survives in both. Schema-1 rows, which have
    no `unit_key`, fall back to `args_digest` (lower-case hex SHA-256 of every
    argument followed by one NUL byte). Both keys pin the same test vectors in
    Rust and Python, so neither side can drift silently.

WHAT IT REPORTS
    * `joined`: estimates matched to a journal row that carries a measured
      peak. When several journal rows share a digest, the largest peak wins,
      because that is the one admission must not under-predict.
    * `unmeasured`: matched, but no journal row for the digest has a peak (a
      cache hit, an older zccache, or a platform that could not measure).
    * `unjoined`: no journal row at all for the digest.
    * `under_predicted` and `worst_under_predictions`: rows whose measured
      peak exceeds the estimate, ranked by measured/estimate.
    * `measured_source`: `tree` when any joined measurement used zccache
      1.13.24's `tree_peak_rss_bytes` (compiler plus live descendants, so a
      linker grandchild counts), otherwise `child` (the compiler alone).
    * `history`: schema-3 rows score the per-unit history estimator
      (soldr#3152 step 4) the same way: `joined` units had a remembered
      `history_tree_peak_bytes`, `without_history` were first sightings, and
      `under_predicted` / `max_measured_over_history` measure how far the
      remembered peak fell short of the compile's measured peak.
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


def unit_key(args: list[str]) -> str | None:
    """`<crate name>/<cargo -C metadata>`, or None without both parts."""
    name = None
    metadata = None
    index = 0
    while index < len(args):
        arg = args[index]
        value = None
        if arg == "--crate-name" and index + 1 < len(args):
            name = args[index + 1]
            index += 1
        elif arg.startswith("--crate-name="):
            name = arg.split("=", 1)[1]
        elif arg == "-C" and index + 1 < len(args):
            value = args[index + 1]
            index += 1
        elif arg.startswith("-C") and len(arg) > 2:
            value = arg[2:]
        if value is not None and value.startswith("metadata="):
            metadata = value.split("=", 1)[1]
        index += 1
    if name is None or metadata is None:
        return None
    return f"{name}/{metadata}"


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


def _merge_peak(peaks: dict[str, int | None], key: str, peak: object) -> None:
    current = peaks.get(key)
    if isinstance(peak, int) and peak > 0:
        peaks[key] = peak if current is None else max(current, peak)
    else:
        peaks.setdefault(key, None)


def _journal_peaks(
    journals: Iterable[Path],
) -> tuple[dict[str, int | None], dict[str, int | None]]:
    """(child peaks, tree peaks) per join key; None when never measured.

    Each journal row is indexed under both its `args_digest` and, when cargo
    supplied one, its `unit_key`, so every estimate schema joins from the same
    maps.
    """
    child: dict[str, int | None] = {}
    tree: dict[str, int | None] = {}
    for journal in journals:
        for row in _read_jsonl(journal):
            args = row.get("args")
            if not isinstance(args, list) or not all(isinstance(a, str) for a in args):
                continue
            keys = [args_digest(args)]
            key = unit_key(args)
            if key is not None:
                keys.append(key)
            for join_key in keys:
                _merge_peak(child, join_key, row.get("child_peak_rss_bytes"))
                _merge_peak(tree, join_key, row.get("tree_peak_rss_bytes"))
    return child, tree


def measured_peaks(journals: Iterable[Path]) -> dict[str, int | None]:
    """Largest measured compiler-child peak per join key (schema-1/2 view)."""
    return _journal_peaks(journals)[0]


def analyze(
    estimate_logs: Iterable[Path], journal_paths: Iterable[Path], top: int = 10
) -> dict[str, Any]:
    """Join estimates to measured peaks and summarise prediction error."""
    child_peaks, tree_peaks = _journal_peaks(discover_journals(journal_paths))
    estimates = 0
    joined = 0
    unmeasured = 0
    unjoined = 0
    used_tree = False
    pairs: list[dict[str, Any]] = []
    history_pairs: list[tuple[int, int]] = []
    without_history = 0
    for log in estimate_logs:
        for row in _read_jsonl(log):
            estimate = row.get("estimate_bytes")
            key = row.get("unit_key") or row.get("args_digest")
            if not isinstance(estimate, int) or not isinstance(key, str):
                continue
            estimates += 1
            if key not in child_peaks:
                unjoined += 1
                continue
            tree = tree_peaks.get(key)
            measured = tree if tree is not None else child_peaks[key]
            if measured is None:
                unmeasured += 1
                continue
            used_tree = used_tree or tree is not None
            joined += 1
            if row.get("schema_version", 1) >= 3:
                history = row.get("history_tree_peak_bytes")
                if isinstance(history, int) and history > 0:
                    history_pairs.append((history, measured))
                else:
                    without_history += 1
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
    history_ratios = [measured / history for history, measured in history_pairs]
    return {
        "estimates": estimates,
        "joined": joined,
        "unmeasured": unmeasured,
        "unjoined": unjoined,
        "measured_source": "tree" if used_tree else "child",
        "under_predicted": len(under),
        "max_measured_over_estimate": max(ratios) if ratios else None,
        "worst_under_predictions": under[:top],
        "history": {
            "joined": len(history_pairs),
            "without_history": without_history,
            "under_predicted": sum(
                1 for history, measured in history_pairs if measured > history
            ),
            "max_measured_over_history": (
                max(history_ratios) if history_ratios else None
            ),
        },
    }


def _format_text(report: dict[str, Any]) -> str:
    lines = [
        f"estimates: {report['estimates']}",
        f"joined (measured peak): {report['joined']}",
        f"unmeasured (no peak on any matching journal row): {report['unmeasured']}",
        f"unjoined (no matching journal row): {report['unjoined']}",
        f"measured source: {report['measured_source']}",
        f"under-predicted: {report['under_predicted']}",
        f"max measured/estimate: {report['max_measured_over_estimate']}",
    ]
    history = report["history"]
    lines.append(
        f"history: joined {history['joined']}, first sightings "
        f"{history['without_history']}, under-predicted "
        f"{history['under_predicted']}, max measured/history "
        f"{history['max_measured_over_history']}"
    )
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
