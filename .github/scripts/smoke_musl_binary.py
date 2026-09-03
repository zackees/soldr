#!/usr/bin/env python3
"""Smoke-test the standalone musl release binary (soldr#2469 step 2.2).

The musllinux wheel smoke proves pip installation. This independent gate keeps
covering the statically-linked binary uploaded in the release archive and
checks both the ``--version`` and ``version --json`` dispatch paths.

Usage (CI):
    python3 .github/scripts/smoke_musl_binary.py \
        --target x86_64-unknown-linux-musl --binary soldr --expected-version v0.9.2
"""

from __future__ import annotations

import argparse
import os
import subprocess
import sys
from pathlib import Path

from release_artifacts import normalized_release_version, version_json_status


class MuslBinarySmokeError(RuntimeError):
    """The standalone musl binary is missing or violates its CLI contract."""


def binary_path(target: str, binary: str, target_dir: Path) -> Path:
    return target_dir / target / "release" / binary


def expected_version(version: str) -> str:
    return normalized_release_version(version)


def version_problem(output: str) -> str | None:
    if output.startswith("soldr "):
        return None
    return (
        "musl binary's 'soldr --version' output did not start with 'soldr ' "
        "— likely shipping a stub binary (soldr#1140)."
    )


def version_json_problem(output: str, expected: str) -> str | None:
    status = version_json_status(output, expected)
    if status == "empty":
        return (
            "musl binary's 'soldr version --json' produced empty stdout (soldr#1202)."
        )
    if status in {"mismatch", "invalid"}:
        return (
            "musl binary's 'soldr version --json' output does not include "
            f"soldr_version={expected} (soldr#1202)."
        )
    return None


def run_cli(command: list[str], *, capture: bool) -> str:
    try:
        completed = subprocess.run(
            command, check=True, capture_output=capture, text=capture
        )
    except subprocess.CalledProcessError as error:
        stderr = error.stderr.strip() if isinstance(error.stderr, str) else ""
        stdout = error.stdout.strip() if isinstance(error.stdout, str) else ""
        diagnostic = stderr or stdout or str(error)
        raise MuslBinarySmokeError(
            f"musl binary CLI probe failed ({' '.join(command)}):\n{diagnostic}"
        ) from error
    return completed.stdout if capture else ""


def print_file_metadata(path: Path) -> None:
    """Print file(1) diagnostics without making that optional tool a prerequisite."""
    try:
        subprocess.run(["file", str(path)], check=False)
    except OSError as error:
        print(f"musl binary smoke: could not run file(1): {error}", file=sys.stderr)


def smoke_binary(*, target: str, binary: str, expected: str, target_dir: Path) -> None:
    path = binary_path(target, binary, target_dir)
    if not path.is_file() or not os.access(path, os.X_OK):
        raise MuslBinarySmokeError(f"missing executable release binary: {path}")
    print_file_metadata(path)
    version_output = run_cli([str(path), "--version"], capture=True)
    problem = version_problem(version_output)
    if problem:
        raise MuslBinarySmokeError(problem)
    print(f"musl smoke test — soldr --version output: {version_output.strip()}")

    json_output = run_cli([str(path), "version", "--json"], capture=True)
    problem = version_json_problem(json_output, expected)
    if problem:
        raise MuslBinarySmokeError(problem)
    print(f"musl smoke test — soldr version --json output: {json_output.strip()}")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--expected-version", required=True)
    parser.add_argument("--target-dir", type=Path, default=Path("target"))
    args = parser.parse_args(argv)
    try:
        smoke_binary(
            target=args.target,
            binary=args.binary,
            expected=expected_version(args.expected_version),
            target_dir=args.target_dir,
        )
    except (MuslBinarySmokeError, OSError) as error:
        print(str(error), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
