#!/usr/bin/env python3
"""Enforce zero host-platform selection outside soldr-platform (#2493).

A hand-written production, test, example, or bench source file violates the boundary when it contains a
host-platform `#[cfg]` / `#[cfg_attr]` / `cfg!()` invocation outside the
concrete platform trees, or (outside crates/soldr-platform entirely) a
direct reference to `platform_imp` / `platform_win` / `platform_linux` /
`platform_macos`.
"""

from __future__ import annotations

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
CFG_STARTS = ("#[cfg(", "#[cfg_attr(", "#![cfg(", "#![cfg_attr(", "cfg!(")
CONCRETE_TREES = ("platform_imp", "platform_win", "platform_linux", "platform_macos")
NATIVE_MARKERS = (
    "std::os::windows",
    "std::os::unix",
    "std::os::linux",
    "std::os::macos",
    "windows_sys",
    "windows::Win32",
    "libc::",
    "tokio::net::windows",
    "tokio::net::Unix",
    "interprocess::os::windows",
    "interprocess::os::unix",
)
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


def native_platform_references(masked_source: str) -> list[str]:
    """Native OS APIs and extension traits forbidden outside concrete trees."""
    return [marker for marker in NATIVE_MARKERS if marker in masked_source]


def boundary_sources() -> list[Path]:
    """Every hand-written workspace Rust source file, repo-relative."""
    files: list[Path] = []
    for path in SOURCE_ROOT.glob("**/*.rs"):
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
    for path in boundary_sources():
        if is_boundary(path):
            continue
        try:
            source = path.read_text(encoding="utf-8")
        except OSError:
            continue
        masked = mask_comments_and_strings(source)
        if platform_cfg_invocations(masked) or native_platform_references(masked):
            found.add(path.as_posix())
            continue
        if not path.as_posix().startswith("crates/soldr-platform/"):
            if concrete_tree_references(masked):
                found.add(path.as_posix())
    return found


def main() -> int:
    found = violations()
    if found:
        print("platform-cfg boundary failed:")
        for path in sorted(found):
            print(f"  host selection outside boundary: {path}")
        return 1
    print("platform-cfg boundary: zero violations")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
