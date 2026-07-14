#!/usr/bin/env python3
"""Force and validate a warm zccache restore for Windows MSVC CI outputs."""

from __future__ import annotations

import argparse
import os
import stat
import subprocess
from pathlib import Path


FIRST_PARTY_PACKAGES = (
    "soldr-cli",
    "soldr-core",
    "soldr-fetch",
    "soldr-cache",
    "soldr-daemon",
)
ARCHIVE_FILTER = "!binary(/cli_cargo_/) and !binary(/cli_daemon_/) and !binary(/cli_rust_plan/)"


def run(command: list[str]) -> None:
    print("+", " ".join(command), flush=True)
    subprocess.run(command, check=True)


def clean_first_party(*, target: str, profile: str) -> None:
    command = ["soldr", "cargo", "clean"]
    for package in FIRST_PARTY_PACKAGES:
        command.extend(["--package", package])
    command.extend(["--target", target, "--profile", profile])
    run(command)


def validate_pe(path: Path, *, context: str) -> None:
    if not path.is_file() or path.stat().st_size < 2:
        raise SystemExit(f"{context} did not produce {path}")
    if path.read_bytes()[:2] != b"MZ":
        raise SystemExit(f"{context} produced a non-PE artifact at {path}")
    print(f"{context}: valid PE artifact {path} ({path.stat().st_size} bytes)")


def archive_members(path: Path) -> list[str]:
    if not path.is_file() or path.stat().st_size == 0:
        raise SystemExit(f"nextest archive is missing or empty: {path}")
    result = subprocess.run(
        ["tar", "--list", "--file", str(path)],
        check=True,
        capture_output=True,
        text=True,
    )
    members = result.stdout.splitlines()
    if not any(member.lower().endswith(".exe") for member in members):
        raise SystemExit(f"nextest archive contains no Windows .exe test artifact: {path}")
    return members


def build_roundtrip(*, target: str, profile: str) -> None:
    artifact = Path("target") / target / profile / "soldr.exe"
    validate_pe(artifact, context="cold cached build")
    clean_first_party(target=target, profile=profile)
    if artifact.exists():
        raise SystemExit(f"cargo clean left the warm-restore probe in place: {artifact}")
    run(
        [
            "soldr",
            "build",
            "--profile",
            profile,
            "--target",
            target,
            "--package",
            "soldr-cli",
            "--bin",
            "soldr",
        ]
    )
    validate_pe(artifact, context="warm cached rebuild")


def archive_roundtrip(*, target: str, profile: str, archive: Path) -> None:
    members = archive_members(archive)
    print(f"cold cached archive: {len(members)} members including Windows executables")

    # Cleaning first-party test outputs also removes Stage B's validated CLI.
    # Preserve that exact PE while forcing nextest to request every test output
    # from zccache again; Stage D packages it after this probe.
    soldr_artifact = Path("target") / target / profile / "soldr.exe"
    saved_soldr = soldr_artifact.read_bytes()
    saved_mode = stat.S_IMODE(soldr_artifact.stat().st_mode)
    clean_first_party(target=target, profile=profile)
    archive.unlink()
    run(
        [
            "soldr",
            "cargo",
            "nextest",
            "archive",
            "--cargo-profile",
            profile,
            "--target",
            target,
            "--workspace",
            "--archive-file",
            str(archive),
            "--archive-format",
            "tar-zst",
            "-E",
            ARCHIVE_FILTER,
        ]
    )
    members = archive_members(archive)
    print(f"warm cached archive: {len(members)} members including Windows executables")
    if not soldr_artifact.exists():
        soldr_artifact.parent.mkdir(parents=True, exist_ok=True)
        soldr_artifact.write_bytes(saved_soldr)
        os.chmod(soldr_artifact, saved_mode)
    validate_pe(soldr_artifact, context="preserved Stage B")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--phase", required=True, choices=("build", "archive"))
    parser.add_argument("--target", required=True)
    parser.add_argument("--profile", required=True)
    parser.add_argument("--archive", type=Path)
    args = parser.parse_args()

    if not args.target.endswith("-pc-windows-msvc"):
        raise SystemExit(f"cache roundtrip only supports Windows MSVC: {args.target}")
    if not os.environ.get("ZCCACHE_CACHE_DIR"):
        raise SystemExit("ZCCACHE_CACHE_DIR must select the job-local cache")

    if args.phase == "build":
        build_roundtrip(target=args.target, profile=args.profile)
    else:
        if args.archive is None:
            parser.error("--archive is required for --phase archive")
        archive_roundtrip(target=args.target, profile=args.profile, archive=args.archive)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
