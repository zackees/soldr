#!/usr/bin/env python3
"""Reject literal bare-Cargo process launches in Nextest integration sources."""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
PATTERN = r'Command\s*::\s*new\s*\(\s*"cargo"\s*\)'


def integration_test_roots(repo_root: Path = REPO_ROOT) -> tuple[Path, ...]:
    return tuple(sorted((repo_root / "crates").glob("*/tests")))


def ripgrep_bare_cargo(repo_root: Path = REPO_ROOT) -> tuple[str, ...]:
    roots = integration_test_roots(repo_root)
    if not roots:
        return ()
    command = [
        "rg",
        "--line-number",
        "--with-filename",
        "--glob",
        "*.rs",
        PATTERN,
        *(str(root) for root in roots),
    ]
    result = subprocess.run(
        command,
        cwd=repo_root,
        text=True,
        capture_output=True,
        check=False,
    )
    if result.returncode not in {0, 1}:
        detail = result.stderr.strip() or f"ripgrep exited {result.returncode}"
        raise RuntimeError(detail)
    return tuple(line for line in result.stdout.splitlines() if line)


def main() -> int:
    try:
        matches = ripgrep_bare_cargo()
    except (FileNotFoundError, RuntimeError) as error:
        print(f"Nextest bare-Cargo guard could not run ripgrep: {error}", file=sys.stderr)
        return 2
    if matches:
        print("Nextest bare-Cargo guard failed:", file=sys.stderr)
        for match in matches:
            print(f"  {match}", file=sys.stderr)
        print(
            "intentional nested Cargo must launch the CARGO environment capability",
            file=sys.stderr,
        )
        return 1
    print("Nextest bare-Cargo guard passed (ripgrep found no literal launches)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
