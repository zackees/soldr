#!/usr/bin/env python3
"""Provision one explicitly required rustup target through packaged Soldr.

The target-run reusable workflow installs the repository's pinned toolchain
with the target-native Soldr artifact.  A few archive-replay regressions need
an additional rust standard library that is intentionally not in the
repository-wide ``rust-toolchain.toml``.  Keep that lane-local mutation here:
the workflow supplies the channel it already proved with ``toolchain ensure``
and this helper invokes *that same packaged Soldr binary*.

Usage:
    python .github/scripts/target_run_add_target.py \
      --soldr /path/to/soldr --channel 1.95.0 --target wasm32-wasip1-threads
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from collections.abc import Callable, Sequence
from pathlib import Path
from typing import Any


def required_value(value: str) -> str:
    """Accept a nonblank workflow argument, never a silent default."""

    value = value.strip()
    if not value:
        raise argparse.ArgumentTypeError("value must not be empty")
    return value


def provision_target(
    soldr: Path,
    *,
    channel: str,
    target: str,
    run: Callable[..., Any] = subprocess.run,
) -> None:
    """Install ``target`` in the already-provisioned ``channel`` via Soldr."""

    command: Sequence[str] = [
        str(soldr),
        "rustup",
        "target",
        "add",
        target,
        "--toolchain",
        channel,
    ]
    run(command, check=True)


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--soldr", type=Path, required=True)
    parser.add_argument("--channel", type=required_value, required=True)
    parser.add_argument("--target", type=required_value, required=True)
    args = parser.parse_args(argv)

    try:
        provision_target(args.soldr, channel=args.channel, target=args.target)
    except (OSError, subprocess.CalledProcessError) as error:
        print(
            f"failed to provision {args.target} through Soldr: {error}", file=sys.stderr
        )
        return 1

    print(
        f"target-run toolchain: provisioned {args.target} for {args.channel} through Soldr"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
