#!/usr/bin/env python3
"""Summarize Cargo JSON unit freshness and enforce first-party expectations."""

import argparse
import json
from pathlib import Path
import sys


def summarize(path: Path, root_package_id: str) -> dict[str, int]:
    units: list[dict[str, object]] = []
    for line_number, line in enumerate(
        path.read_text(encoding="utf-8").splitlines(), 1
    ):
        try:
            message = json.loads(line)
        except json.JSONDecodeError as exc:
            raise ValueError(
                f"{path}:{line_number}: invalid Cargo JSON: {exc}"
            ) from exc
        if message.get("reason") == "compiler-artifact":
            units.append(message)

    fresh = sum(unit.get("fresh") is True for unit in units)
    dirty = len(units) - fresh
    first_party_dirty = sum(
        unit.get("fresh") is not True and unit.get("package_id") == root_package_id
        for unit in units
    )
    return {
        "fresh_units": fresh,
        "dirty_units": dirty,
        # Cargo emits one dirty compiler-artifact per compiler unit. This is
        # a JSON-level proxy, not a direct wrapper process counter.
        "compiler_invocations": dirty,
        "first_party_dirty_units": first_party_dirty,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("log", type=Path)
    parser.add_argument("--root-package-id", required=True)
    parser.add_argument("--expect-first-party-dirty", type=int, required=True)
    args = parser.parse_args()

    try:
        result = summarize(args.log, args.root_package_id)
    except (OSError, ValueError) as exc:
        print(f"cargo_units: {exc}", file=sys.stderr)
        return 2

    actual = result["first_party_dirty_units"]
    if actual != args.expect_first_party_dirty:
        print(
            "cargo_units: expected exactly "
            f"{args.expect_first_party_dirty} dirty first-party unit(s); got {actual}",
            file=sys.stderr,
        )
        return 1
    print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
