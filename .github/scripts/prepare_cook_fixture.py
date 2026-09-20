#!/usr/bin/env python3
"""Materialize path dependencies required by the standalone zccache fixture.

The fixture is a checkout of `zackees/zccache` pinned to the exact crate
version soldr embeds. Which layout it needs is read from that checkout's own
`[patch.crates-io]` table rather than assumed: zccache has moved its vendored
`notify` patch between a sibling (`../notify`) and an in-repo path
(`vendor/notify`), and a hard-coded assumption silently stops matching when it
moves again. soldr#3301 was the previous spelling of that failure, where the
script demanded a `vendor/notify` tree the pinned revision no longer needed.
"""

from __future__ import annotations

import argparse
import shutil
import tomllib
from pathlib import Path


def notify_patch_path(fixture: Path) -> Path | None:
    """Return the path zccache's `notify` patch points at, or None.

    The value is resolved against the fixture root exactly as cargo resolves
    it, so `../notify` becomes a sibling of the checkout and `vendor/notify`
    stays inside it.
    """
    manifest = fixture / "Cargo.toml"
    if not manifest.is_file():
        raise SystemExit(f"fixture has no Cargo.toml: {manifest}")
    patch = tomllib.loads(manifest.read_text(encoding="utf-8"))
    entry = patch.get("patch", {}).get("crates-io", {}).get("notify")
    if not isinstance(entry, dict):
        return None
    path = entry.get("path")
    if not path:
        return None
    return (fixture / path).resolve()


def prepare_fixture(fixture: Path) -> Path | None:
    """Materialize the notify path dependency when the fixture needs one.

    Returns the directory that was created, or None when the pinned revision
    already carries everything cargo will look for.
    """
    fixture = fixture.resolve()
    destination = notify_patch_path(fixture)
    if destination is None:
        return None
    if destination.is_relative_to(fixture):
        # The patch points inside the checkout, so cargo already has it. A
        # missing tree here is a broken pin, not something to paper over.
        if not (destination / "Cargo.toml").is_file():
            raise SystemExit(
                f"fixture patches notify at {destination}, but that tree is missing"
            )
        return None

    # The patch names a sibling of the checkout, which only exists inside
    # soldr (`_vender/notify`). Materialize it from the vendored copy.
    source = fixture / "vendor" / "notify"
    if not (source / "Cargo.toml").is_file():
        raise SystemExit(f"missing vendored notify fixture at {source}")
    if destination.exists():
        raise SystemExit(f"notify fixture destination already exists: {destination}")
    shutil.copytree(source, destination)
    return destination


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixture", required=True, type=Path)
    args = parser.parse_args()
    destination = prepare_fixture(args.fixture)
    if destination is None:
        print("zccache fixture needs no notify path dependency; nothing to prepare")
    else:
        print(f"prepared zccache notify path dependency at {destination}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
