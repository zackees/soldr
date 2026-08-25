#!/usr/bin/env python3
"""Reject implicit per-lint Cargo target directories in CI (soldr#2865)."""

from __future__ import annotations

import argparse
from pathlib import Path


def implicit_dylint_target_dirs(repo_root: Path) -> list[Path]:
    """Return standalone Dylint targets Cargo creates without ``--target-dir``."""

    return sorted(path for path in repo_root.glob("dylints/*/target") if path.is_dir())


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--repo-root",
        type=Path,
        default=Path(__file__).resolve().parents[2],
        help="checkout root to inspect (default: this script's repository)",
    )
    args = parser.parse_args()
    implicit = implicit_dylint_target_dirs(args.repo_root)
    if not implicit:
        return 0

    paths = "\n".join(f"- {path.relative_to(args.repo_root)}" for path in implicit)
    parser.error(
        "Dylint test steps created implicit per-lint target directories:\n"
        f"{paths}\nUse the shared nightly-keyed target/dylint/tests directory."
    )
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
