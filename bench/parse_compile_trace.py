#!/usr/bin/env python3
"""Bucket and analyze a SOLDR_DAEMON_TRACE JSONL file.

Reads the JSONL trace file produced by the soldr daemon (when the
`SOLDR_DAEMON_TRACE` env var was set during the run) and prints
per-phase aggregate statistics: count, total micros, p50/p95/p99, and
share of the total dispatch budget.

This is the analysis half of the soldr#981 cold-build diagnostic plan.
We expect the output to identify the per-compile dispatch phase that
the failed zccache#939 buffer-elimination plan missed.

Usage:
  python bench/parse_compile_trace.py PATH_TO_TRACE.jsonl
"""

from __future__ import annotations

import json
import os
import statistics
import sys
from collections import defaultdict
from pathlib import Path
from typing import Dict, List


def percentile(values: List[float], pct: float) -> float:
    if not values:
        return 0.0
    if len(values) == 1:
        return values[0]
    s = sorted(values)
    k = (len(s) - 1) * pct
    f = int(k)
    c = min(f + 1, len(s) - 1)
    if f == c:
        return s[f]
    return s[f] + (s[c] - s[f]) * (k - f)


def main(argv: List[str]) -> int:
    if len(argv) != 2:
        print(__doc__.strip(), file=sys.stderr)
        return 64
    path = Path(argv[1])
    if not path.exists():
        print(f"error: {path} does not exist", file=sys.stderr)
        return 65

    phase_values: Dict[str, List[int]] = defaultdict(list)
    bad_lines = 0
    total_lines = 0
    with path.open("r", encoding="utf-8") as fh:
        for raw in fh:
            raw = raw.strip()
            if not raw:
                continue
            total_lines += 1
            try:
                obj = json.loads(raw)
                phase = obj["phase"]
                micros = int(obj["micros"])
            except (json.JSONDecodeError, KeyError, ValueError):
                bad_lines += 1
                continue
            phase_values[phase].append(micros)

    if not phase_values:
        print("no records parsed", file=sys.stderr)
        return 1

    timing_keys = {"inner_compile", "wire_stdout", "wire_stderr", "wire_done", "total_dispatch", "inner_compile_err"}
    counter_keys = {"stdout_bytes", "stderr_bytes"}

    total_dispatch = phase_values.get("total_dispatch", [])
    total_grand_us = sum(total_dispatch)

    print(f"trace file       : {path}")
    print(f"lines parsed     : {total_lines - bad_lines} (bad: {bad_lines})")
    print(f"compiles         : {len(total_dispatch)}")
    print(f"total dispatch s : {total_grand_us / 1e6:.3f}")
    print()
    print(f"{'phase':<22} {'count':>7} {'sum_ms':>10} {'p50_us':>9} {'p95_us':>9} {'p99_us':>9} {'%total':>7}")
    print("-" * 80)

    for phase in sorted(timing_keys):
        vals = phase_values.get(phase, [])
        if not vals:
            continue
        s = sum(vals)
        pct = (s / total_grand_us * 100.0) if total_grand_us else 0.0
        print(
            f"{phase:<22} {len(vals):>7} "
            f"{s/1000:>10.1f} "
            f"{percentile(vals, 0.50):>9.0f} "
            f"{percentile(vals, 0.95):>9.0f} "
            f"{percentile(vals, 0.99):>9.0f} "
            f"{pct:>6.1f}%"
        )

    print()
    print("byte counters:")
    for phase in sorted(counter_keys):
        vals = phase_values.get(phase, [])
        if not vals:
            continue
        s = sum(vals)
        print(
            f"  {phase:<20} count={len(vals):>6}  total={s/1e6:>8.2f} MB  "
            f"p50={percentile(vals, 0.50):>9.0f} B  "
            f"p99={percentile(vals, 0.99):>9.0f} B"
        )

    print()
    print("any phase not in the timing/counter sets above:")
    for phase, vals in sorted(phase_values.items()):
        if phase in timing_keys or phase in counter_keys:
            continue
        print(f"  {phase:<20} count={len(vals):>6}  sum={sum(vals)}")

    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
