#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Bump the project version in Cargo.toml (single source of truth).

All other version references derive from it automatically:
  - Rust crates: workspace inheritance (version.workspace = true)
  - Rust internal dependencies: release publish flow stamps exact pins
  - Maturin Python packages: dynamic = ["version"] reads Cargo.toml
  - Root pyproject.toml: setup.py reads Cargo.toml at build time

Usage:
    ./bump patch          # 1.2.11 -> 1.2.12
    ./bump minor          # 1.2.11 -> 1.3.0
    ./bump major          # 1.2.11 -> 2.0.0
    ./bump 3.0.0          # set exact version
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CARGO_TOML = ROOT / "Cargo.toml"

VERSION_RE = re.compile(
    r'(\[workspace\.package\]\s*\nversion\s*=\s*")([^"]+)(")'
)


def read_version() -> str:
    match = VERSION_RE.search(CARGO_TOML.read_text())
    if not match:
        print("ERROR: Could not find [workspace.package] version in Cargo.toml", file=sys.stderr)
        sys.exit(1)
    return match.group(2)


def bump(current: str, part: str) -> str:
    parts = list(map(int, current.split(".")))
    if len(parts) != 3:
        print(f"ERROR: Current version {current!r} is not semver (major.minor.patch)", file=sys.stderr)
        sys.exit(1)
    if part == "patch":
        parts[2] += 1
    elif part == "minor":
        parts[1] += 1
        parts[2] = 0
    elif part == "major":
        parts[0] += 1
        parts[1] = 0
        parts[2] = 0
    else:
        print(f"ERROR: Unknown bump type {part!r}", file=sys.stderr)
        sys.exit(1)
    return ".".join(map(str, parts))


def write_version(new_version: str) -> None:
    text = CARGO_TOML.read_text()
    # Update [workspace.package] version
    text, count = VERSION_RE.subn(rf"\g<1>{new_version}\3", text)
    if count != 1:
        print("ERROR: Failed to update workspace version in Cargo.toml", file=sys.stderr)
        sys.exit(1)
    CARGO_TOML.write_text(text)


def main() -> None:
    if len(sys.argv) != 2:
        print("Usage: ./bump <patch|minor|major|X.Y.Z>")
        sys.exit(1)

    arg = sys.argv[1]
    current = read_version()

    if arg in ("patch", "minor", "major"):
        new_version = bump(current, arg)
    elif re.fullmatch(r"\d+\.\d+\.\d+", arg):
        new_version = arg
    else:
        print(f"ERROR: Argument must be patch, minor, major, or X.Y.Z (got {arg!r})")
        sys.exit(1)

    if new_version == current:
        print(f"Version is already {current}")
        return

    write_version(new_version)
    print(f"{current} -> {new_version}")


if __name__ == "__main__":
    main()
