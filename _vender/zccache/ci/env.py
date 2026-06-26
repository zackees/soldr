"""Shared process environment helpers for CI scripts."""

from __future__ import annotations

import os
import shutil
import subprocess

WINDOWS_MSVC_SUFFIX = "-pc-windows-msvc"


def clean_env() -> dict[str, str]:
    env = os.environ.copy()
    env.pop("VIRTUAL_ENV", None)
    return env


def rustc_host() -> str | None:
    soldr = shutil.which("soldr")
    if soldr is None:
        raise FileNotFoundError(
            "Cannot find soldr on PATH. Install the global soldr tool before "
            "running Rust development commands."
        )

    result = subprocess.run(
        [soldr, "--no-cache", "rustc", "-vV"],
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        env=clean_env(),
    )
    if result.returncode != 0:
        return None

    for line in result.stdout.splitlines():
        if line.startswith("host:"):
            return line.split(":", 1)[1].strip()
    return None


def ensure_windows_msvc() -> None:
    if os.name != "nt":
        return

    host = rustc_host()
    if host is None:
        raise RuntimeError("Failed to resolve rustc host via soldr.")
    if not host.endswith(WINDOWS_MSVC_SUFFIX):
        raise RuntimeError(
            "Windows Rust toolchain resolved to "
            f"{host}, expected an MSVC host ending in {WINDOWS_MSVC_SUFFIX}. "
            "Ensure rustup's cargo bin is used instead of Chocolatey or GNU shims."
        )
