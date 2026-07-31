#!/usr/bin/env python3
"""Fail CI when a host-resolved toolchain ran under soldr's managed homes (soldr#1799).

soldr keeps private managed `RUSTUP_HOME`/`CARGO_HOME` for the nightly dylint
needs. #1768 was the bug where that managed environment leaked onto *host*
tool executions. It does not fail loudly: flipping homes between runs changes
which rustc is used, which invalidates cargo's fingerprints and zccache's keys,
so warm builds silently recompile the world -- "10-50x slower, indefinitely".

The build log already records the pair that makes this checkable
(`build_log.rs`): each `<toolchain home_origin=... binary=... />` row says
which homes were applied and which binary actually ran. The invariant the code
documents is:

    `home_origin="managed"` is only legitimate when `binary` physically lives
    inside a managed home.

So a row claiming `managed` for a binary outside the managed root is exactly
the #1768 leak. `caller` and `repo-local` both mean the caller's own homes were
used (they differ only in reporting), so neither is constrained here.

Usage:
    python3 .github/scripts/check_toolchain_homes.py <log.xml|dir> --managed-root ~/.soldr
Options:
    --managed-root PATH   the soldr root whose homes count as "managed" (repeatable)
"""

from __future__ import annotations

import argparse
import re
from pathlib import Path

TOOLCHAIN_ROW = re.compile(
    r'<toolchain\s+home_origin="(?P<origin>[^"]*)"\s+binary="(?P<binary>[^"]*)"',
)


def normalize(path: str) -> str:
    """Case-fold and strip Windows' `\\\\?\\` extended-length prefix.

    Build logs on Windows record the canonicalized path, which carries that
    prefix; a plain prefix comparison against the root would miss otherwise.
    """
    cleaned = path.strip()
    for marker in ("\\\\?\\UNC\\", "\\\\?\\"):
        if cleaned.startswith(marker):
            cleaned = cleaned[len(marker) :]
            break
    return cleaned.replace("\\", "/").rstrip("/").casefold()


def parse_rows(xml: str) -> "list[tuple[str, str]]":
    """Return `(home_origin, binary)` for every toolchain row in one log."""
    return [(m.group("origin"), m.group("binary")) for m in TOOLCHAIN_ROW.finditer(xml)]


def violations(rows, managed_roots) -> "list[str]":
    """Rows claiming `managed` for a binary outside every managed root.

    An unknown origin is not a violation: a newer soldr may add a value, and
    this guard must not fail a build over a discriminant it has not learned.
    """
    roots = [normalize(root) for root in managed_roots if str(root).strip()]
    found = []
    for origin, binary in rows:
        if origin != "managed":
            continue
        target = normalize(binary)
        if not any(target.startswith(root + "/") or target == root for root in roots):
            found.append(
                f'home_origin="managed" but the binary is outside every managed '
                f"root: {binary}"
            )
    return found


def _logs(target: Path) -> "list[Path]":
    if target.is_dir():
        return sorted(target.rglob("*.xml"))
    return [target] if target.exists() else []


def main(argv: "list[str] | None" = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("target", help="a build-log XML file, or a directory of them")
    parser.add_argument(
        "--managed-root",
        action="append",
        default=[],
        help="soldr root whose homes count as managed (repeatable)",
    )
    args = parser.parse_args(argv)

    if not args.managed_root:
        print("check_toolchain_homes: no --managed-root given; nothing to check")
        return 0

    logs = _logs(Path(args.target))
    if not logs:
        # No logs is a wiring question, not a build failure -- say so and pass,
        # so the guard cannot become a mysterious red on its own plumbing.
        print(f"check_toolchain_homes: no build logs under {args.target}")
        return 0

    failed = []
    checked = 0
    for log in logs:
        try:
            rows = parse_rows(log.read_text(encoding="utf-8", errors="replace"))
        except OSError as error:
            print(f"check_toolchain_homes: could not read {log}: {error}")
            continue
        checked += len(rows)
        for message in violations(rows, args.managed_root):
            failed.append(f"{log}: {message}")

    if failed:
        print(
            "check_toolchain_homes: soldr's managed toolchain homes leaked onto a "
            "host-resolved tool (soldr#1799/#1768). That silently invalidates cargo "
            "fingerprints and zccache keys, so warm builds recompile the world:"
        )
        for message in failed:
            print(f"  - {message}")
        return 1

    print(
        f"check_toolchain_homes: {checked} toolchain row(s) across {len(logs)} log(s) — OK"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
