#!/usr/bin/env python3
"""Smoke-test a musllinux release wheel in a stock Alpine container.

This is the downstream proof that pip sees the PEP 656 wheel tag and installs
its binary rather than falling back to a source build.  It also exercises the
installed console script's ``--version`` and ``version --json`` contracts.

Usage (CI):
    python3 .github/scripts/smoke_musllinux_wheel.py --expected-version v0.9.2
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from pathlib import Path

ALPINE_IMAGE = "alpine:3.20"
ALPINE_SMOKE = r"""apk add --no-cache python3 py3-pip
python3 -m venv /tmp/venv
. /tmp/venv/bin/activate
pip install --no-index --only-binary=:all: --find-links /dist "soldr==${EXPECTED_VERSION}"
out="$(soldr --version)"
printf "alpine wheel smoke test - soldr --version output: %s\n" "$out"
case "$out" in
  "soldr "*) ;;
  *) echo "ERROR: soldr --version output did not start with soldr " >&2; exit 1 ;;
esac
json_out="$(soldr version --json)"
test -n "$json_out"
printf "alpine wheel smoke test - soldr version --json output: %s\n" "$json_out"
compact="$(printf "%s" "$json_out" | tr -d " \n\r\t")"
case "$compact" in
  *"\"soldr_version\":\"${EXPECTED_VERSION}\""*) ;;
  *) echo "ERROR: soldr version --json did not include soldr_version=${EXPECTED_VERSION}" >&2; exit 1 ;;
esac"""


class MusllinuxWheelSmokeError(RuntimeError):
    """The Alpine wheel smoke could not mount the staged release wheels."""


def expected_version(version: str) -> str:
    return version.removeprefix("v")


def docker_command(*, expected: str, dist: Path) -> list[str]:
    resolved_dist = dist.resolve()
    return [
        "docker",
        "run",
        "--rm",
        "-e",
        f"EXPECTED_VERSION={expected}",
        "-v",
        f"{resolved_dist}:/dist:ro",
        ALPINE_IMAGE,
        "sh",
        "-euxc",
        ALPINE_SMOKE,
    ]


def smoke_musllinux_wheel(*, expected: str, dist: Path) -> None:
    if not dist.is_dir():
        raise MusllinuxWheelSmokeError(f"wheel dist directory does not exist: {dist}")
    subprocess.run(docker_command(expected=expected, dist=dist), check=True)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--expected-version", required=True)
    parser.add_argument("--dist", type=Path, default=Path("dist"))
    args = parser.parse_args(argv)
    try:
        smoke_musllinux_wheel(
            expected=expected_version(args.expected_version),
            dist=args.dist,
        )
    except (MusllinuxWheelSmokeError, OSError, subprocess.CalledProcessError) as error:
        print(str(error), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
