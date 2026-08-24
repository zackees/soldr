#!/usr/bin/env python3
"""Print the zccache version resolved in Cargo.lock."""

from __future__ import annotations

import argparse
import re
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]


def locked_zccache_version(repo_root: Path) -> str:
    lock_path = repo_root / "Cargo.lock"
    text = lock_path.read_text(encoding="utf-8")
    matches = re.findall(
        r'^name = "zccache"\n^version = "([^"]+)"',
        text,
        flags=re.MULTILINE,
    )
    if len(matches) != 1:
        raise ValueError(
            f"expected exactly one zccache package in {lock_path}, found {len(matches)}"
        )
    return matches[0]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo-root", type=Path, default=REPO_ROOT)
    args = parser.parse_args()
    print(locked_zccache_version(args.repo_root.resolve()))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
