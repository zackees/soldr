#!/usr/bin/env python3
"""Prepare a release wheel build, then launch the pinned Maturin builder.

The release workflow needs a clean wheel output directory, a metadata check
against the candidate version, and the setup-soldr wheel-hook fallback before
it invokes ``build_release_wheel.py``.  Keeping that policy in Python makes
both its error paths and the runner-specific Soldr driver selection testable.

Usage (CI):
    python3 .github/scripts/prepare_release_wheel.py \
        --target x86_64-unknown-linux-gnu --runner-os Linux \
        --expected-version v0.9.2 --wheel-hook "python -m build --wheel"
"""

from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import sys
from pathlib import Path

from release_artifacts import normalized_release_version, runner_binary_suffix

DEFAULT_WHEEL_HOOK = "python -m build --wheel"
BUILD_SCRIPT = Path(__file__).with_name("build_release_wheel.py")


class WheelPreparationError(RuntimeError):
    """A release-wheel precondition was missing or inconsistent."""


def driver_path(runner_os: str, driver_dir: Path) -> Path:
    """Return the host Soldr binary used by this release runner."""
    return driver_dir / f"soldr{runner_binary_suffix(runner_os)}"


def expected_package_version(version: str) -> str:
    return normalized_release_version(version)


def clean_wheel_outputs(repo_root: Path) -> None:
    """Discard wheel output that could make the release package stale bytes."""
    dist_dir = repo_root / "dist"
    target_dir = repo_root / "target"
    for wheel in dist_dir.glob("*.whl"):
        wheel.unlink()
    for directory in (target_dir / "wheels", target_dir / "maturin"):
        shutil.rmtree(directory, ignore_errors=True)
    for wheel in target_dir.rglob("*.whl"):
        wheel.unlink()
    dist_dir.mkdir(parents=True, exist_ok=True)


def best_effort(command: list[str], *, cwd: Path) -> None:
    """Run cleanup commands whose historical workflow deliberately tolerated failure."""
    subprocess.run(command, cwd=cwd, check=False)


def installed_package_version(driver: Path, *, cwd: Path) -> str:
    """Ask the release driver for the workspace facade package version."""
    completed = subprocess.run(
        [
            str(driver),
            "cargo",
            "metadata",
            "--no-deps",
            "--format-version",
            "1",
            "--manifest-path",
            "crates/soldr-cli/Cargo.toml",
        ],
        cwd=cwd,
        check=False,
        capture_output=True,
        text=True,
    )
    if completed.returncode != 0:
        return ""
    try:
        packages = json.loads(completed.stdout).get("packages", [])
    except json.JSONDecodeError:
        return ""
    return next(
        (
            str(package.get("version", ""))
            for package in packages
            if package.get("name") == "soldr-cli"
        ),
        "",
    )


def validate_workspace_version(
    driver: Path, expected_version: str, *, cwd: Path
) -> None:
    """Refuse to build a wheel from a manifest that disagrees with the release."""
    observed = installed_package_version(driver, cwd=cwd)
    if not observed:
        raise WheelPreparationError("cargo metadata returned no version for soldr-cli")
    expected = expected_package_version(expected_version)
    if observed != expected:
        raise WheelPreparationError(
            f"soldr-cli version ({observed}) does not match release version ({expected})"
        )


def resolved_hook(configured_hook: str) -> str:
    return configured_hook.strip() or DEFAULT_WHEEL_HOOK


def builder_command(target: str, hook: str) -> list[str]:
    return [
        "uv",
        "run",
        "--no-project",
        "--python",
        "3.13",
        "--with",
        "build",
        "python",
        str(BUILD_SCRIPT),
        "--target",
        target,
        "--hook",
        hook,
    ]


def prepare_and_build(
    *,
    target: str,
    runner_os: str,
    expected_version: str,
    wheel_hook: str,
    repo_root: Path,
    driver_dir: Path,
) -> None:
    """Clean, validate, and launch Soldr's source-built Maturin wheel helper."""
    driver = driver_path(runner_os, driver_dir)
    clean_wheel_outputs(repo_root)
    best_effort(
        [
            str(driver),
            "cargo",
            "clean",
            "-p",
            "soldr-cli",
            "--target",
            target,
            "--release",
        ],
        cwd=repo_root,
    )
    best_effort(
        [
            "git",
            "restore",
            "--",
            "Cargo.toml",
            "Cargo.lock",
            "crates/soldr-cli/Cargo.toml",
        ],
        cwd=repo_root,
    )
    validate_workspace_version(driver, expected_version, cwd=repo_root)
    hook = resolved_hook(wheel_hook)
    print(f"setup-soldr wheel hook: {hook}", flush=True)
    subprocess.run(["uv", "python", "install", "3.13"], cwd=repo_root, check=True)
    subprocess.run(builder_command(target, hook), cwd=repo_root, check=True)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True)
    parser.add_argument(
        "--runner-os", required=True, choices=["Linux", "macOS", "Windows"]
    )
    parser.add_argument("--expected-version", required=True)
    parser.add_argument("--wheel-hook", default="")
    parser.add_argument("--repo-root", type=Path, default=Path("."))
    parser.add_argument("--driver-dir", type=Path, default=Path("target/release"))
    args = parser.parse_args(argv)
    try:
        prepare_and_build(
            target=args.target,
            runner_os=args.runner_os,
            expected_version=args.expected_version,
            wheel_hook=args.wheel_hook,
            repo_root=args.repo_root,
            driver_dir=args.driver_dir,
        )
    except (OSError, subprocess.CalledProcessError, WheelPreparationError) as error:
        print(str(error), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
