"""Helpers for running this workspace through soldr."""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
from pathlib import Path

from ci.env import clean_env

REPO_ROOT = Path(__file__).parent.parent.resolve()


def soldr_executable() -> str:
    soldr = shutil.which("soldr")
    if soldr is None:
        print(
            "error: `soldr` not found on PATH. Install the global soldr tool "
            "before running Rust development commands.",
            file=sys.stderr,
        )
        raise SystemExit(1)
    return soldr


def self_build_env() -> dict[str, str]:
    return clean_env()


def cargo_command(*args: str) -> list[str]:
    return [soldr_executable(), "cargo", *args]


def rust_tool_command(tool: str, *args: str) -> list[str]:
    return [soldr_executable(), tool, *args]


def run_workspace_cargo(args: list[str]) -> int:
    result = subprocess.run(
        cargo_command(*args),
        text=True,
        encoding="utf-8",
        errors="replace",
        cwd=str(REPO_ROOT),
        env=self_build_env(),
    )
    return result.returncode


def _run_cargo_bin(package: str) -> None:
    extra = sys.argv[1:]
    if extra and extra[0] == "--":
        extra = extra[1:]

    cmd = cargo_command("run", "-p", package)
    if extra:
        cmd.append("--")
        cmd.extend(extra)

    result = subprocess.run(
        cmd,
        text=True,
        encoding="utf-8",
        errors="replace",
        cwd=str(REPO_ROOT),
        env=self_build_env(),
    )
    sys.exit(result.returncode)


def run_zccache() -> None:
    _run_cargo_bin("zccache-cli")


def run_zccache_daemon() -> None:
    _run_cargo_bin("zccache-daemon")


def run_zccache_fingerprint() -> None:
    _run_cargo_bin("zccache-fp")


def check_on_stop() -> None:
    _run_cargo_bin("zccache-ci")
