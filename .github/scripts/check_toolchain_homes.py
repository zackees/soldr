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
from itertools import pairwise
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


def repo_key(log_name: str) -> str:
    """The repository part of a build-log filename.

    Build logs are named `<UTC timestamp>-<sanitized-cwd>.xml`, so stripping
    the leading timestamp leaves a stable per-repository key. Grouping by it
    is not cosmetic: different repositories legitimately resolve different
    toolchains (a repo-local `.cargo/bin/cargo` versus soldr's managed rustup
    toolchain), so comparing across them would report a "flip" on every
    interleaved build and drown the real signal.
    """
    stem = log_name[:-4] if log_name.endswith(".xml") else log_name
    head, sep, tail = stem.partition("-")
    # Only treat the head as a timestamp when it looks like one.
    if sep and head[:8].isdigit():
        return tail
    return stem


def find_flips(logs: "list[tuple[str, list[tuple[str, str]]]]") -> "list[str]":
    """Report where a repository's toolchain changed between consecutive builds.

    soldr#1799's "flag the known causes" -- a home flip or a compiler-path
    change between runs is what invalidates cargo's fingerprints and zccache's
    keys, so a warm build recompiles the world. Seeing it named beside the two
    builds is the difference between diagnosing that in a minute and spending
    a full pass on it.

    Diagnostic only: this reports, it does not decide. Sorting is by filename,
    whose UTC timestamp prefix makes lexicographic order chronological.
    """
    by_repo: "dict[str, list[tuple[str, tuple[str, str]]]]" = {}
    for name, rows in logs:
        for row in rows:
            by_repo.setdefault(repo_key(name), []).append((name, row))

    flips = []
    for repo in sorted(by_repo):
        entries = sorted(by_repo[repo], key=lambda item: item[0])
        for (prev_name, prev), (name, current) in pairwise(entries):
            if prev == current:
                continue
            what = []
            if prev[0] != current[0]:
                what.append(f"home_origin {prev[0]} -> {current[0]}")
            if prev[1] != current[1]:
                what.append(f"binary {prev[1]} -> {current[1]}")
            flips.append(f"{repo}: {', '.join(what)} (between {prev_name} and {name})")
    return flips


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
    parser.add_argument(
        "--report-flips",
        action="store_true",
        help=(
            "also report where a repository's toolchain changed between "
            "consecutive builds (diagnostic; never changes the exit code)"
        ),
    )
    args = parser.parse_args(argv)

    if not args.managed_root and not args.report_flips:
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
    parsed: "list[tuple[str, list[tuple[str, str]]]]" = []
    for log in logs:
        try:
            rows = parse_rows(log.read_text(encoding="utf-8", errors="replace"))
        except OSError as error:
            print(f"check_toolchain_homes: could not read {log}: {error}")
            continue
        checked += len(rows)
        parsed.append((log.name, rows))
        for message in violations(rows, args.managed_root):
            failed.append(f"{log}: {message}")

    if args.report_flips:
        flips = find_flips(parsed)
        if flips:
            print(
                "check_toolchain_homes: the toolchain changed between consecutive "
                "builds of the same repository (soldr#1799). A home flip or a "
                "compiler-path change invalidates cargo fingerprints and zccache "
                "keys, so the next warm build recompiles the world:"
            )
            for message in flips:
                print(f"  - {message}")
        else:
            print(
                "check_toolchain_homes: no toolchain flips between consecutive builds"
            )

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
