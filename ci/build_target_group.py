#!/usr/bin/env python3
"""Named target-group build script.

Builds a named subset of canonical targets (e.g. win-mac-musl) in one
invocation, collecting artifacts into a local dist/ directory.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

GROUPS: dict[str, list[str]] = {
    "win-mac-musl": ["win-x64", "mac-arm64", "linux-x64-musl", "linux-arm64-musl"],
}


def load_aliases() -> dict[str, str]:
    """Load alias → triple mapping from ci/canonical-targets.json."""
    script_dir = Path(__file__).resolve().parent
    canonical_path = script_dir / "canonical-targets.json"
    with open(canonical_path, "r", encoding="utf-8") as f:
        data = json.load(f)
    return data.get("aliases", data)


def resolve_target(alias: str) -> str:
    """Resolve a canonical alias to its Rust target triple."""
    aliases = load_aliases()
    triple = aliases.get(alias)
    if triple is None:
        raise ValueError(f"Unknown alias '{alias}' not found in canonical-targets.json")
    return triple


@dataclass(frozen=True)
class BuildPlan:
    alias: str
    triple: str
    command: list[str]
    artifact_paths: list[Path]


def compute_plan(group: str, out_dir: Path, extra_args: list[str]) -> list[BuildPlan]:
    """Compute the build plan for a given group."""
    if group not in GROUPS:
        raise ValueError(f"Unknown group '{group}'. Available: {list(GROUPS.keys())}")

    plans = []
    for alias in GROUPS[group]:
        triple = resolve_target(alias)
        cmd = ["soldr", "build", "--release", "--target", alias, *extra_args]
        # Artifact paths: soldr and soldr-daemon, with .exe extension on windows
        suffix = ".exe" if triple.startswith("x86_64-pc-windows-msvc") or triple.startswith("aarch64-pc-windows-msvc") else ""
        artifact_dir = out_dir / group / alias
        artifact_paths = [
            artifact_dir / (f"soldr{suffix}"),
            artifact_dir / (f"soldr-daemon{suffix}"),
        ]
        plans.append(BuildPlan(
            alias=alias,
            triple=triple,
            command=cmd,
            artifact_paths=artifact_paths,
        ))
    return plans


def check_host() -> None:
    """Warn if running on Windows host (Linux targets won't build)."""
    if sys.platform == "win32":
        print(
            "WARNING: Running on Windows host. Building Linux musl targets will fail "
            "(see #2315). Consider running on a Linux host or container.",
            file=sys.stderr,
        )


def run_builds(plans: list[BuildPlan], keep_going: bool) -> None:
    """Execute the build plan, collecting results."""
    failed = []
    for plan in plans:
        print(f"Building {plan.alias} ({plan.triple})...")
        try:
            subprocess.run(plan.command, check=True, cwd=os.getcwd())
        except subprocess.CalledProcessError as e:
            print(f"Failed: {plan.alias} ({e.returncode})", file=sys.stderr)
            if not keep_going:
                sys.exit(1)
            failed.append(plan.alias)
    if failed and keep_going:
        print(f"Completed with failures: {', '.join(failed)}", file=sys.stderr)
        sys.exit(1)


def main() -> None:
    parser = argparse.ArgumentParser(description="Build named target groups")
    parser.add_argument("--group", default="win-mac-musl", help="Group name to build")
    parser.add_argument("--out-dir", default="dist", help="Output directory for artifacts")
    parser.add_argument("--dry-run", action="store_true", help="Print plan and exit")
    parser.add_argument("--keep-going", action="store_true", help="Build all targets even if some fail")
    parser.add_argument("extra", nargs=argparse.REMAINDER, help="Extra arguments forwarded to soldr")
    args = parser.parse_args()

    check_host()

    try:
        plans = compute_plan(args.group, Path(args.out_dir), args.extra)
    except ValueError as e:
        print(str(e), file=sys.stderr)
        sys.exit(1)

    if args.dry_run:
        for plan in plans:
            print(f"ALIAS: {plan.alias}")
            print(f"TRIPLE: {plan.triple}")
            print(f"COMMAND: {' '.join(plan.command)}")
            print(f"ARTIFACTS: {', '.join(str(p) for p in plan.artifact_paths)}")
            print("---")
        sys.exit(0)

    # Create artifact directories
    for plan in plans:
        for p in plan.artifact_paths:
            p.parent.mkdir(parents=True, exist_ok=True)

    run_builds(plans, args.keep_going)


if __name__ == "__main__":
    main()
