#!/usr/bin/env python3
"""Resolve the `triplets` workflow_dispatch input into a build matrix.

soldr#1664: `vcpkg-windows-refresh.yml` advertised a `triplets` input
("Comma-separated triplets to refresh") but the matrix hard-coded both
values, so an operator who asked for one triplet still paid for a full
Windows port build of both. This script is what makes the input real.

Per CLAUDE.md, non-trivial workflow logic lives in `.github/scripts/`
rather than inline YAML, so the parsing and validation are unit-testable
without pushing a branch.

Usage:

    python3 .github/scripts/vcpkg_triplet_matrix.py \
        --triplets "x64-windows-static-md" \
        --output "$GITHUB_OUTPUT"

Writes `matrix=<json>` for consumption via `fromJSON`. With no
`--output`, prints the JSON to stdout so it is runnable from a shell.

An unknown triplet is a hard error: silently ignoring it would build
something other than what the operator asked for, and a typo would look
like a successful refresh that quietly skipped a bundle.
"""

from __future__ import annotations

import argparse
import json
import sys

# The triplets this workflow knows how to build. Adding one here is not
# enough on its own — the pin list in `.github/vcpkg-pin.txt` has to
# have port coverage for it too.
SUPPORTED_TRIPLETS: tuple[str, ...] = (
    "x64-windows-static-md",
    "arm64-windows-static-md",
)


def parse_triplets(raw: str) -> list[str]:
    """Parse a comma-separated triplet list into a validated matrix.

    Empty or whitespace-only input means "all supported triplets",
    matching the input's documented default. Duplicates collapse while
    preserving the order the operator wrote, so the job list is
    predictable.

    Raises ValueError naming every unknown entry at once, rather than
    failing on the first, so an operator fixing a typo sees the whole
    problem in one run.
    """
    if raw is None or not raw.strip():
        return list(SUPPORTED_TRIPLETS)

    requested: list[str] = []
    for chunk in raw.split(","):
        triplet = chunk.strip()
        if not triplet:
            # Tolerate "a,,b" and a trailing comma — a stray separator
            # is a typo, not a request for an empty build.
            continue
        if triplet not in requested:
            requested.append(triplet)

    if not requested:
        return list(SUPPORTED_TRIPLETS)

    unknown = [t for t in requested if t not in SUPPORTED_TRIPLETS]
    if unknown:
        raise ValueError(
            f"unknown triplet(s): {', '.join(unknown)}. "
            f"Supported: {', '.join(SUPPORTED_TRIPLETS)}"
        )
    return requested


def build_matrix(raw: str) -> dict[str, list[str]]:
    return {"triplet": parse_triplets(raw)}


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--triplets",
        default="",
        help="comma-separated triplets; empty means all supported",
    )
    parser.add_argument(
        "--output",
        default=None,
        help="GITHUB_OUTPUT file to append `matrix=<json>` to",
    )
    args = parser.parse_args(argv)

    try:
        matrix = build_matrix(args.triplets)
    except ValueError as exc:
        print(f"vcpkg_triplet_matrix: {exc}", file=sys.stderr)
        return 1

    payload = json.dumps(matrix, separators=(",", ":"))
    print(f"matrix={payload}")
    if args.output:
        with open(args.output, "a", encoding="utf-8") as handle:
            handle.write(f"matrix={payload}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
