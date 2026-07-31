#!/usr/bin/env python3
"""Fail CI when an immediately-repeated build recompiles anything (soldr#1799).

The silent failure #1799 exists to catch is a *fingerprint* invalidation: a
toolchain-home flip, or a compiler-path change, makes cargo and zccache treat
already-built work as stale, so a warm build recompiles the world. Nothing
errors -- the build is just 10-50x slower, indefinitely.

Re-running the identical build command immediately after it succeeded is the
cheapest possible probe for that. Nothing has changed in between, so cargo must
report zero `Compiling` lines; locally a warm repeat finishes in under a
second. Any crate compiling on that second pass means something invalidated a
fingerprint, which is exactly the condition to surface.

The parser tolerates soldr's elapsed-time line prefixes (soldr#1802), because
the build output it reads is usually stamped.

Usage:
    soldr cargo build ... > warm.log 2>&1
    python3 .github/scripts/check_warm_rebuild.py warm.log
Options:
    --allow N   tolerate up to N recompiled crates (default 0)
"""

from __future__ import annotations

import argparse
import re
from pathlib import Path

# Colour can wrap the verb on either side, so escapes are stripped before
# matching rather than woven into the pattern.
ANSI = re.compile(r"\x1b\[[0-9;]*m")
# Both verbs mean real work was done: `cargo build` says `Compiling`, and
# `cargo check` says `Checking`. Matching only one silently passes for the
# other, which is worse than no guard. The optional leading elapsed stamp
# (`   12.34 `) is soldr's line stamping (soldr#1802).
COMPILING = re.compile(
    r"^(?:\s*\d+\.\d+\s+)?\s*(?:Compiling|Checking)\s+(?P<crate>[A-Za-z0-9_.\-]+)"
)


def recompiled_crates(output: str) -> "list[str]":
    """Crates cargo reported compiling, in order, de-duplicated.

    `Fresh` lines (cargo's verbose form for "nothing to do") are deliberately
    not matched -- only real compilation counts.
    """
    seen: "list[str]" = []
    for raw in output.splitlines():
        match = COMPILING.match(ANSI.sub("", raw))
        if not match:
            continue
        crate = match.group("crate")
        if crate not in seen:
            seen.append(crate)
    return seen


def main(argv: "list[str] | None" = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("log", help="output of the repeated build")
    parser.add_argument(
        "--allow",
        type=int,
        default=0,
        help="tolerate up to N recompiled crates (default 0)",
    )
    args = parser.parse_args(argv)

    path = Path(args.log)
    try:
        output = path.read_text(encoding="utf-8", errors="replace")
    except OSError as error:
        # A missing log is a wiring problem, not a build failure -- say so and
        # pass, so this guard cannot become a mysterious red on its own bug.
        print(f"check_warm_rebuild: could not read {path}: {error}")
        return 0

    crates = recompiled_crates(output)
    if len(crates) <= args.allow:
        print(
            f"check_warm_rebuild: {len(crates)} crate(s) recompiled on the warm "
            f"repeat (allowed {args.allow}) — OK"
        )
        return 0

    print(
        f"check_warm_rebuild: {len(crates)} crate(s) recompiled on a build that "
        "should have been a no-op (soldr#1799). Something invalidated cargo's "
        "fingerprints between two identical builds -- a toolchain-home flip or a "
        "compiler-path change is the usual cause, and it makes every warm build "
        "recompile the world:"
    )
    for crate in crates[:20]:
        print(f"  - {crate}")
    if len(crates) > 20:
        print(f"  ... and {len(crates) - 20} more")
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
