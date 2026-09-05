#!/usr/bin/env python3
"""Ensure Dylint UI-test artifacts use CI's one shared target (soldr#2865)."""

from __future__ import annotations

import argparse
from pathlib import Path

HARMLESS_BOOKKEEPING_FILES = frozenset(
    {
        ".rustc_info.json",
        "CACHEDIR.TAG",
        "debug/.cargo-lock",
        "release/.cargo-lock",
    }
)


def implicit_dylint_target_dirs(repo_root: Path) -> list[Path]:
    """Return local Dylint targets, including bookkeeping-only ones."""

    return sorted(path for path in repo_root.glob("dylints/*/target") if path.is_dir())


def local_dylint_target_artifacts(repo_root: Path) -> list[Path]:
    """Return local target files other than harmless bookkeeping."""

    artifacts = []
    for target_dir in implicit_dylint_target_dirs(repo_root):
        for path in target_dir.rglob("*"):
            if not path.is_file():
                continue
            relative = path.relative_to(target_dir).as_posix()
            # soldr's zero-byte scrub marker (and lock) is bookkeeping too.
            if relative not in HARMLESS_BOOKKEEPING_FILES and not path.name.startswith(
                ".soldr-"
            ):
                artifacts.append(path)
    return sorted(artifacts)


def has_materialized_shared_dependencies(shared_target: Path) -> bool:
    """Return whether native test dependencies were materialized centrally."""

    deps = shared_target / "debug" / "deps"
    return deps.is_dir() and any(path.is_file() for path in deps.iterdir())


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--repo-root",
        type=Path,
        default=Path(__file__).resolve().parents[2],
        help="checkout root to inspect (default: this script's repository)",
    )
    parser.add_argument(
        "--shared-target",
        type=Path,
        required=True,
        help="nightly-keyed target directory shared by every Dylint UI test",
    )
    args = parser.parse_args()
    artifacts = local_dylint_target_artifacts(args.repo_root)
    if artifacts:
        paths = "\n".join(f"- {path.relative_to(args.repo_root)}" for path in artifacts)
        parser.error(
            "Dylint test steps created local compiler artifacts:\n"
            f"{paths}\nUse the shared nightly-keyed target/dylint/tests directory."
        )
    if not has_materialized_shared_dependencies(args.shared_target):
        parser.error(
            "Dylint tests did not materialize dependencies in the shared target:\n"
            f"- {args.shared_target / 'debug' / 'deps'}"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
