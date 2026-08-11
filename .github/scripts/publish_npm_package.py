#!/usr/bin/env python3
"""Idempotently publish the npm package from a validated release checkout."""

from __future__ import annotations

import argparse
import json
import subprocess
from collections.abc import Sequence
from pathlib import Path


def run_npm(
    arguments: Sequence[str], source_dir: Path
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["npm", *arguments],
        cwd=source_dir,
        check=False,
        capture_output=True,
        text=True,
    )


def publish(source_dir: Path) -> bool:
    package = json.loads((source_dir / "package.json").read_text(encoding="utf-8"))
    package_spec = f"{package['name']}@{package['version']}"
    lookup = run_npm(["view", package_spec, "version", "--json"], source_dir)
    if lookup.returncode == 0:
        try:
            published_version = json.loads(lookup.stdout)
        except json.JSONDecodeError:
            published_version = None
        if published_version == package["version"]:
            print(f"{package_spec} is already published; skipping npm publish.")
            return False

    result = run_npm(["publish"], source_dir)
    if result.stdout:
        print(result.stdout, end="")
    if result.returncode != 0:
        detail = result.stderr.strip() or "npm publish failed without an error message"
        raise RuntimeError(detail)
    return True


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-dir", type=Path, default=Path.cwd())
    args = parser.parse_args()
    publish(args.source_dir.resolve())
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, KeyError, TypeError, RuntimeError, json.JSONDecodeError) as exc:
        raise SystemExit(f"npm publication failed: {exc}") from exc
