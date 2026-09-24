#!/usr/bin/env python3
"""Provision and execute the pinned Apple Silicon rust-objcopy before replay.

The macOS ARM release-archive replay launches nested Cargo fixtures.  The
host toolchain can contain rust-objcopy without the matching libLLVM.dylib,
which makes Cargo's strip warning a Soldr build failure several minutes into
the test run.  The llvm-tools component supplies the runtime library; invoking
the exact tool ahead of replay proves it is loadable in this environment.
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from collections.abc import Callable, Sequence
from pathlib import Path
from typing import Any

HOST = "aarch64-apple-darwin"


def provision_objcopy(
    soldr: Path,
    *,
    channel: str,
    rustc: Path,
    run: Callable[..., Any] = subprocess.run,
) -> Path:
    """Install LLVM tools through Soldr and smoke the pinned tool directly."""

    if not channel.strip() or not rustc.is_absolute():
        raise ValueError("channel must be nonblank and rustc must be an absolute path")
    install: Sequence[str] = [
        str(soldr),
        "rustup",
        "component",
        "add",
        "--toolchain",
        channel,
        "llvm-tools-preview",
    ]
    run(install, check=True)
    objcopy = rustc.parent.parent / "lib" / "rustlib" / HOST / "bin" / "rust-objcopy"
    if not objcopy.is_file():
        raise FileNotFoundError(f"pinned toolchain has no rust-objcopy: {objcopy}")
    run([str(objcopy), "--version"], check=True)
    return objcopy


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--soldr", required=True, type=Path)
    parser.add_argument("--channel", required=True)
    parser.add_argument("--rustc", required=True, type=Path)
    args = parser.parse_args(argv)
    try:
        objcopy = provision_objcopy(args.soldr, channel=args.channel, rustc=args.rustc)
    except (OSError, subprocess.CalledProcessError, ValueError) as error:
        print(f"native ARM rust-objcopy runtime preflight failed: {error}", file=sys.stderr)
        return 1
    print(f"native ARM rust-objcopy runtime ready: {objcopy}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
