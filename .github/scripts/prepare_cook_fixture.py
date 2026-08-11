#!/usr/bin/env python3
"""Materialize path dependencies required by the standalone zccache fixture."""

from __future__ import annotations

import argparse
import shutil
from pathlib import Path


def prepare_fixture(fixture: Path) -> Path:
    fixture = fixture.resolve()
    source = fixture / "vendor" / "notify"
    if not (source / "Cargo.toml").is_file():
        raise SystemExit(f"missing vendored notify fixture at {source}")

    # zccache's workspace patch is `notify = { path = "../notify" }`. Inside
    # soldr the submodule naturally has `_vender/notify` as that sibling; a
    # standalone CI checkout needs the same shape materialized explicitly.
    destination = fixture.parent / "notify"
    if destination.exists():
        raise SystemExit(f"notify fixture destination already exists: {destination}")
    shutil.copytree(source, destination)
    return destination


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixture", required=True, type=Path)
    args = parser.parse_args()
    destination = prepare_fixture(args.fixture)
    print(f"prepared zccache notify path dependency at {destination}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
