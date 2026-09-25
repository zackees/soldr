#!/usr/bin/env python3
"""Smoke-test a Linux binary inside an old-glibc container (soldr#1060).

`verify_glibc_baseline.py` proves a binary's *declared* symbol floor by
reading its dynamic-symbol version requirements with `readelf -V`. That is
necessary but not sufficient: a binary can satisfy every individual symbol
check and still fail to actually start on an old distro for a reason
`readelf` cannot see (a missing shared library entirely, a loader quirk, a
syscall that behaves differently at runtime). soldr#1060 item 3 asks for
"the -gnu artifact actually launches on a glibc-2.17-era environment" as a
real end-to-end check, not just a static one.

This script runs the binary's `--version` inside a container old enough to
carry glibc 2.17 (`quay.io/pypa/manylinux2014_x86_64`, itself built on
CentOS 7 / glibc 2.17 -- the same baseline the `manylinux_2_17` wheel tag
and soldr's own glibc ceiling name) and asserts it exits 0 and prints
something. It never tries to interpret glibc symbol versions itself --
that stays `verify_glibc_baseline.py`'s job -- it only proves the binary
*runs*.

Usage:
    python3 .github/scripts/glibc_container_smoke.py <binary> [<binary> ...]
    python3 .github/scripts/glibc_container_smoke.py --image IMAGE <binary>

Exits non-zero if docker is unavailable, the container cannot be started, or
any binary fails to run inside it.
"""

from __future__ import annotations

import argparse
import shutil
import subprocess
import sys
from pathlib import Path

# CentOS 7 based, glibc 2.17 -- the exact floor soldr#1060 targets and the
# same baseline `manylinux_2_17` names. Pulled from quay.io (not Docker Hub)
# because it is the canonical home for PyPA's manylinux images and does not
# hit Docker Hub's anonymous-pull rate limit on shared CI runners.
DEFAULT_IMAGE = "quay.io/pypa/manylinux2014_x86_64"


def run_smoke(binary: Path, image: str) -> None:
    if not binary.is_file():
        raise SystemExit(f"glibc_container_smoke: no such file: {binary}")

    mount_target = "/soldr-under-test"
    command = [
        "docker",
        "run",
        "--rm",
        "--platform",
        "linux/amd64",
        "-v",
        f"{binary.resolve()}:{mount_target}:ro",
        image,
        mount_target,
        "--version",
    ]
    print(f"glibc_container_smoke: $ {' '.join(command)}", flush=True)
    result = subprocess.run(command, capture_output=True, text=True, check=False)
    print(result.stdout, end="")
    print(result.stderr, end="", file=sys.stderr)
    if result.returncode != 0:
        raise SystemExit(
            f"glibc_container_smoke: {binary} failed to run inside {image} "
            f"(exit {result.returncode}). This is the exact failure mode "
            "soldr#1060 item 3 names: a -gnu artifact whose glibc-baseline "
            "check passes statically but cannot actually start on an "
            "old-glibc host."
        )
    if not result.stdout.strip():
        raise SystemExit(
            f"glibc_container_smoke: {binary} exited 0 inside {image} but "
            "printed nothing to stdout -- `--version` should always print a "
            "version string, so an empty result is itself suspicious rather "
            "than a pass."
        )
    print(
        f"glibc_container_smoke: {binary} ran successfully inside {image} "
        f"-> {result.stdout.strip()!r}"
    )


def main(argv: "list[str] | None" = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--image",
        default=DEFAULT_IMAGE,
        help=f"Container image to run the binary inside (default: {DEFAULT_IMAGE}).",
    )
    parser.add_argument("binaries", nargs="+", type=Path)
    args = parser.parse_args(argv)

    if shutil.which("docker") is None:
        print("glibc_container_smoke: docker is not on PATH", file=sys.stderr)
        return 1

    try:
        for binary in args.binaries:
            run_smoke(binary, args.image)
    except SystemExit as error:
        print(error, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
