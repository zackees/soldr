#!/usr/bin/env python3
"""Run setup-soldr's PEP 517 hook for one explicit release target.

setup-soldr prepares the compiler, linker, SDK, and sysroot and advertises a
generic ``python -m build --wheel`` hook.  The target must be passed as a PEP
517 config setting so the pinned Soldr backend forwards it to Maturin before
Maturin's Cargo metadata probe; environment-only linker settings are too late
and can make a Windows cross-build probe the Linux host with ``lld-link``.
"""

from __future__ import annotations

import argparse
import os
import shlex
import subprocess
import sys
from collections.abc import Mapping, Sequence
from pathlib import Path

RELEASE_TARGETS = frozenset(
    {
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-musl",
        "x86_64-apple-darwin",
        "aarch64-apple-darwin",
        "x86_64-pc-windows-msvc",
        "aarch64-pc-windows-msvc",
    }
)


def validate_target(target: str) -> None:
    """Reject targets outside the canonical release matrix."""

    if target not in RELEASE_TARGETS:
        raise ValueError(f"unsupported release wheel target: {target}")


def build_environment(target: str, base: Mapping[str, str]) -> dict[str, str]:
    """Copy *base* and enforce the release PEP 517 environment."""

    validate_target(target)
    env = dict(base)
    configured_profile = env.get("SOLDR_PEP517_PROFILE", "").strip()
    if configured_profile and configured_profile != "release":
        raise ValueError(
            f"release wheel requires SOLDR_PEP517_PROFILE=release, got {configured_profile!r}"
        )
    env["SOLDR_PEP517_PROFILE"] = "release"
    env["SOLDR_RELEASE_CI"] = "1"
    return env


def read_github_env(path: Path) -> dict[str, str]:
    """Read the simple ``NAME=value`` records emitted by ``soldr prepare``."""

    result: dict[str, str] = {}
    for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if not line:
            continue
        key, separator, value = line.partition("=")
        if not separator or not key:
            raise ValueError(f"invalid GitHub environment record at {path}:{number}")
        result[key] = value
    return result


def run_hook(*, target: str, hook: str, base_env: Mapping[str, str]) -> None:
    command = shlex.split(hook)
    if command != ["python", "-m", "build", "--wheel"]:
        raise ValueError(f"unexpected setup-soldr target wheel hook: {hook!r}")
    # The hook spells the PEP 517 interpreter generically as ``python``.
    # Resolve it to the uv-provisioned interpreter before the prepared target
    # PATH is applied; target preparation intentionally owns PATH and may not
    # retain the frontend environment's shim directory.
    command[0] = sys.executable
    command.extend(("--config-setting", f"target={target}"))
    env = build_environment(target, base_env)
    print(f"setup-soldr wheel target: {target}", flush=True)
    print("Soldr PEP 517 profile: release", flush=True)
    print(f"PEP 517 config setting: target={target}", flush=True)
    subprocess.run(command, check=True, env=env)


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True, choices=sorted(RELEASE_TARGETS))
    parser.add_argument("--hook", required=True)
    parser.add_argument(
        "--github-env",
        type=Path,
        help="Optional soldr prepare --github-env file for local/Docker reproduction.",
    )
    args = parser.parse_args(argv)
    base_env = dict(os.environ)
    if args.github_env:
        base_env.update(read_github_env(args.github_env))
    run_hook(target=args.target, hook=args.hook, base_env=base_env)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
