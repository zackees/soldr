#!/usr/bin/env python3
"""Enforce the exact allowlist for host-platform selection outside the
soldr-platform boundary (issue #2493).

A production source file violates the boundary when it contains a
host-platform `#[cfg]` / `#[cfg_attr]` / `cfg!()` invocation outside the
concrete platform trees, or (outside crates/soldr-platform entirely) a
direct reference to `platform_imp` / `platform_win` / `platform_linux` /
`platform_macos`. The allowlist records the remaining pre-boundary files
and is a ratchet: both missing and stale entries fail.
"""

from __future__ import annotations

import argparse
import re
from pathlib import Path

PLATFORM_SELECTORS = (
    "windows",
    "unix",
    "target_os",
    "target_family",
    "target_arch",
    "target_abi",
    "target_env",
    "target_vendor",
    "target_endian",
    "target_pointer_width",
)
CFG_STARTS = ("#[cfg(", "#[cfg_attr(", "cfg!(")
CONCRETE_TREES = ("platform_imp", "platform_win", "platform_linux", "platform_macos")
BOUNDARY_PREFIXES = (
    "crates/soldr-platform/src/platform_win",
    "crates/soldr-platform/src/platform_linux",
    "crates/soldr-platform/src/platform_macos",
)
SELECTION_SITE = "crates/soldr-platform/src/lib.rs"
SOURCE_ROOT = Path("crates")


def mask_comments_and_strings(source: str) -> str:
    """Replace comments and string literals with whitespace (newlines kept)."""
    output: list[str] = []
    index = 0
    length = len(source)
    while index < length:
        # Raw strings (including byte raw strings).
        raw = re.match(r"b?r(#*)\"", source[index:])
        if raw:
            hashes = raw.group(1)
            start = index
            index += raw.end()
            terminator = '"' + hashes
            end = source.find(terminator, index)
            if end == -1:
                end = length
            else:
                end += len(terminator)
            for char in source[start:end]:
                output.append("\n" if char == "\n" else " ")
            index = end
            continue
        if source.startswith("//", index):
            while index < length and source[index] != "\n":
                output.append(" ")
                index += 1
            continue
        if source.startswith("/*", index):
            depth = 1
            output.append("  ")
            index += 2
            while index < length and depth:
                if source.startswith("/*", index):
                    depth += 1
                    output.append("  ")
                    index += 2
                elif source.startswith("*/", index):
                    depth -= 1
                    output.append("  ")
                    index += 2
                else:
                    output.append("\n" if source[index] == "\n" else " ")
                    index += 1
            continue
        if source[index] == '"':
            output.append(" ")
            index += 1
            while index < length:
                char = source[index]
                output.append("\n" if char == "\n" else " ")
                index += 1
                if char == "\\" and index < length:
                    output.append(" ")
                    index += 1
                elif char == '"':
                    break
            continue
        output.append(source[index])
        index += 1
    return "".join(output)


def balanced_invocation(source: str, offset: int) -> str | None:
    """Return the balanced `(...)` clause starting at `offset`, if any."""
    depth = 0
    saw_open = False
    for relative, char in enumerate(source[offset:]):
        if char == "(":
            saw_open = True
            depth += 1
        elif char == ")" and saw_open:
            depth -= 1
            if depth == 0:
                return source[offset : offset + relative + 1]
    return None


def platform_cfg_invocations(masked_source: str) -> list[str]:
    """Host-platform cfg invocations in pre-expansion source."""
    compact = "".join(masked_source.split())
    invocations: list[str] = []
    for start in CFG_STARTS:
        cursor = 0
        while True:
            offset = compact.find(start, cursor)
            if offset == -1:
                break
            clause = balanced_invocation(compact, offset)
            if clause is None:
                cursor = offset + len(start)
                continue
            if any(selector in clause for selector in PLATFORM_SELECTORS):
                invocations.append(clause.removeprefix("#["))
            cursor = offset + len(clause)
    return invocations


def concrete_tree_references(masked_source: str) -> list[str]:
    """Direct references to the concrete implementation trees."""
    references: list[str] = []
    for name in CONCRETE_TREES:
        for match in re.finditer(r"\b" + re.escape(name) + r"\b", masked_source):
            references.append(match.group(0))
    return references


def production_sources() -> list[Path]:
    """Every workspace production source file, repo-relative."""
    files: list[Path] = []
    for path in SOURCE_ROOT.glob("*/src/**/*.rs"):
        if path.is_file():
            files.append(path)
    files.sort()
    return files


def is_boundary(path: Path) -> bool:
    text = path.as_posix()
    if text == SELECTION_SITE:
        return True
    return any(text.startswith(prefix) for prefix in BOUNDARY_PREFIXES)


def violations() -> set[str]:
    """Repo-relative paths of production files that still select the host."""
    found: set[str] = set()
    for path in production_sources():
        if is_boundary(path):
            continue
        try:
            source = path.read_text(encoding="utf-8")
        except OSError:
            continue
        masked = mask_comments_and_strings(source)
        if platform_cfg_invocations(masked):
            found.add(path.as_posix())
            continue
        if not path.as_posix().startswith("crates/soldr-platform/"):
            if concrete_tree_references(masked):
                found.add(path.as_posix())
    return found


def read_allowlist(path: Path) -> set[str]:
    lines = path.read_text(encoding="utf-8").splitlines()
    return {
        line.strip()
        for line in lines
        if line.strip() and not line.strip().startswith("#")
    }


def verify(allowlist_path: Path) -> list[str]:
    """Report stale and missing allowlist entries."""
    allowlisted = read_allowlist(allowlist_path)
    actual = violations()
    problems: list[str] = []
    for stale in sorted(allowlisted - actual):
        problems.append(f"stale allowlist entry: {stale}")
    for missing in sorted(actual - allowlisted):
        problems.append(f"new boundary violation: {missing}")
    return problems


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--allowlist",
        type=Path,
        default=Path("dylints/ban_platform_cfg_outside_boundary/src/allowlist.txt"),
    )
    args = parser.parse_args()

    problems = verify(args.allowlist)
    if problems:
        print("platform-cfg boundary ratchet failed:")
        for problem in problems:
            print(f"  {problem}")
        return 1
    print(f"platform-cfg boundary ratchet: {len(read_allowlist(args.allowlist))} allowed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
