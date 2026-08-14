#!/usr/bin/env python3
"""In-container half of the win-x64 wheel harness.

Runs `soldr wheel` for the Windows target and copies the produced wheel to
`/out`. Kept in Python rather than shell so the argument handling and the
"did we actually produce a wheel" check are explicit and testable.

Dev profile on purpose: this harness exists to unwedge and test a host, so
build time matters more than binary size. `--release` is available behind a
flag for the rare case where the shipped shape is what needs reproducing.
"""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
from pathlib import Path

WORKSPACE = Path("/work")
OUT_DIR = Path("/out")


def wheel_outputs(target: str) -> list[Path]:
    """Every wheel maturin could have written, newest first.

    maturin writes to `target/wheels`; a cross build lands under the
    target-scoped tree. Both are checked rather than assumed so a layout
    change surfaces as "no wheel found" with the searched paths named.
    """
    candidates = [
        WORKSPACE / "target" / "wheels",
        WORKSPACE / "target" / target / "wheels",
    ]
    found: list[Path] = []
    for directory in candidates:
        if directory.is_dir():
            found.extend(directory.glob("*.whl"))
    return sorted(found, key=lambda path: path.stat().st_mtime, reverse=True)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--target",
        default=os.environ.get("SOLDR_WHEEL_TARGET", "x86_64-pc-windows-msvc"),
    )
    parser.add_argument(
        "--release",
        action="store_true",
        help="build the release profile instead of the default dev profile",
    )
    parser.add_argument(
        "rest",
        nargs=argparse.REMAINDER,
        help="extra arguments forwarded verbatim to `soldr wheel`",
    )
    args = parser.parse_args(argv)

    rest = args.rest
    if rest and rest[0] == "--":
        rest = rest[1:]

    # Record which wheels predate this run so a stale artifact from an earlier
    # invocation can never be reported as this run's output.
    before = {path.resolve() for path in wheel_outputs(args.target)}

    command = ["soldr", "wheel", "--target", args.target]
    if args.release:
        command.append("--release")
    command.extend(rest)

    print(f"+ {' '.join(command)}", flush=True)
    completed = subprocess.run(command, cwd=WORKSPACE, check=False)
    if completed.returncode != 0:
        return completed.returncode

    produced = [path for path in wheel_outputs(args.target) if path.resolve() not in before]
    if not produced:
        # A zero exit with no new wheel means the build silently produced
        # nothing; say so rather than reporting success.
        print(
            "build_wheel: soldr wheel succeeded but wrote no new wheel; searched "
            f"{WORKSPACE / 'target' / 'wheels'} and "
            f"{WORKSPACE / 'target' / args.target / 'wheels'}",
            file=sys.stderr,
        )
        return 1

    OUT_DIR.mkdir(parents=True, exist_ok=True)
    for wheel in produced:
        destination = OUT_DIR / wheel.name
        shutil.copy2(wheel, destination)
        print(f"build_wheel: wrote {destination} ({wheel.stat().st_size} bytes)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
