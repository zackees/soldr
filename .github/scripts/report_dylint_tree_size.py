#!/usr/bin/env python3
"""Report the on-disk size of the Dylint target trees.

soldr#2996 Phase 6 proposes persisting `target/dylint/` as a
`dylint-foundation`-tier cache entry, because it is the only mechanism that
can reach the 638s Dylint block: `Swatinem/rust-cache` deletes those trees at
save time, and `dylint_cook_acceptance.py` shows a per-unit object cache alone
still misses (`object_cache_only: miss`) while a restored tarball skips
(`warm_restored_target: skip`).

That proposal is gated on a number nobody has: how big the trees actually are.
The directive in soldr#2996 admits at most one cache family beyond `soldr
cook`, against a 5 GB budget, so a carve-out has to be costed before it is
granted -- not after.

This reports the three trees separately, because they are not
interchangeable. `soldr dylint cook` prewarms the analysis tree
(`--tree analysis`, the default) and, since soldr#3042, the third-party
dependency layer of the UI-test tree (`--tree tests`). It does not prewarm
`libraries/`, nor the linked UI-test products inside `tests/` -- those are
tier 3 and stay cold by design.

Reports, never fails: a missing tree is information (the stage did not run),
not an error, and this must never be the reason a lane goes red.

Usage:
    python3 .github/scripts/report_dylint_tree_size.py [--target-root target]
"""

from __future__ import annotations

import argparse
import os
import pathlib
import sys

TREES = ("libraries", "target", "tests")


def tree_bytes(path: pathlib.Path) -> tuple[int, int]:
    """Return (total bytes, file count) below *path*, following no symlinks."""
    total = 0
    files = 0
    for root, _dirs, names in os.walk(path, followlinks=False):
        for name in names:
            entry = pathlib.Path(root) / name
            try:
                if entry.is_symlink():
                    continue
                total += entry.stat().st_size
                files += 1
            except OSError:
                # A file that vanished mid-walk is not worth failing over.
                continue
    return total, files


def human(size: int) -> str:
    value = float(size)
    for unit in ("B", "KiB", "MiB", "GiB"):
        if value < 1024 or unit == "GiB":
            return f"{value:.1f} {unit}" if unit != "B" else f"{int(value)} B"
        value /= 1024
    return f"{value:.1f} GiB"


def report(target_root: pathlib.Path) -> list[str]:
    dylint_root = target_root / "dylint"
    lines = ["### Dylint tree sizes (soldr#2996 Phase 6)", ""]
    if not dylint_root.is_dir():
        lines.append(f"No `{dylint_root}` — the Dylint stages did not run.")
        return lines

    lines.append("| tree | size | files |")
    lines.append("|---|---:|---:|")
    grand_total = 0
    grand_files = 0
    for name in TREES:
        path = dylint_root / name
        if not path.is_dir():
            lines.append(f"| `{name}` | absent | — |")
            continue
        size, files = tree_bytes(path)
        grand_total += size
        grand_files += files
        lines.append(f"| `{name}` | {human(size)} | {files} |")
    lines.append(f"| **total** | **{human(grand_total)}** | **{grand_files}** |")
    lines.append("")
    lines.append(
        "Uncompressed. A cache entry would be smaller; treat this as the "
        "ceiling when costing the carve-out against the 5 GB budget."
    )
    return lines


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target-root", default="target")
    args = parser.parse_args(argv)

    lines = report(pathlib.Path(args.target_root))
    body = "\n".join(lines)
    print(body)

    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        try:
            with pathlib.Path(summary).open("a", encoding="utf-8") as handle:
                handle.write(body + "\n")
        except OSError as error:
            print(f"report_dylint_tree_size: summary unwritable: {error}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
