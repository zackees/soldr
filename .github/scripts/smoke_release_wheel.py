#!/usr/bin/env python3
"""Install and smoke-test the release wheel on its build runner.

This gate verifies the installed ``soldr`` console script, rather than merely
asserting that Maturin emitted a wheel.  It catches the soldr#1140 stub-binary
regression and the soldr#1202 ``version --json`` dispatch regression before a
wheel reaches PyPI.

Usage (CI):
    python3 .github/scripts/smoke_release_wheel.py --expected-version v0.9.2
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from pathlib import Path

from release_artifacts import normalized_release_version, version_json_status


class WheelSmokeError(RuntimeError):
    """The release wheel cannot be installed or fails its CLI contract."""


def collect_wheels(dist: Path) -> list[Path]:
    wheels = sorted(dist.glob("*.whl"))
    if not wheels:
        raise WheelSmokeError(f"no wheels in {dist} - maturin produced nothing?")
    return wheels


def console_script(venv: Path) -> Path:
    """Locate the installed console entry point on Unix and Windows virtualenvs."""
    candidates = [venv / "bin" / "soldr", venv / "Scripts" / "soldr.exe"]
    found = next((candidate for candidate in candidates if candidate.is_file()), None)
    if found is None:
        raise WheelSmokeError(
            f"wheel install did not create a soldr console script in {venv}"
        )
    return found


def version_problem(output: str) -> str | None:
    if output.startswith("soldr "):
        return None
    return "wheel's 'soldr --version' output did not start with 'soldr ' — likely shipping a stub binary (soldr#1140)."


def version_json_problem(output: str, expected_version: str) -> str | None:
    status = version_json_status(output, expected_version)
    if status == "empty":
        return "wheel's 'soldr version --json' produced empty stdout (soldr#1202)."
    if status in {"mismatch", "invalid"}:
        return (
            "wheel's 'soldr version --json' output does not include "
            f"soldr_version={expected_version} (soldr#1202)."
        )
    return None


def run_cli(command: list[str]) -> str:
    try:
        completed = subprocess.run(command, check=True, capture_output=True, text=True)
    except subprocess.CalledProcessError as error:
        stderr = error.stderr.strip() if isinstance(error.stderr, str) else ""
        stdout = error.stdout.strip() if isinstance(error.stdout, str) else ""
        diagnostic = stderr or stdout or str(error)
        raise WheelSmokeError(
            f"wheel CLI probe failed ({' '.join(command)}):\n{diagnostic}"
        ) from error
    return completed.stdout


def smoke_wheel(*, expected_version: str, dist: Path, venv: Path) -> None:
    wheels = collect_wheels(dist)
    subprocess.run(["uv", "venv", str(venv)], check=True)
    subprocess.run(
        [
            "uv",
            "pip",
            "install",
            "--python",
            str(venv),
            *(str(wheel) for wheel in wheels),
        ],
        check=True,
    )
    soldr = console_script(venv)

    version_output = run_cli([str(soldr), "--version"])
    problem = version_problem(version_output)
    if problem:
        raise WheelSmokeError(problem)
    print(f"wheel smoke test — soldr --version output: {version_output.strip()}")

    json_output = run_cli([str(soldr), "version", "--json"])
    problem = version_json_problem(
        json_output, normalized_release_version(expected_version)
    )
    if problem:
        raise WheelSmokeError(problem)
    print(f"wheel smoke test — soldr version --json output: {json_output.strip()}")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--expected-version", required=True)
    parser.add_argument("--dist", type=Path, default=Path("dist"))
    parser.add_argument("--venv", type=Path, default=Path(".venv"))
    args = parser.parse_args(argv)
    try:
        smoke_wheel(
            expected_version=args.expected_version, dist=args.dist, venv=args.venv
        )
    except (OSError, subprocess.CalledProcessError, WheelSmokeError) as error:
        print(str(error), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
