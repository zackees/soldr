#!/usr/bin/env python3
from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
from pathlib import Path


def run(command: list[str]) -> None:
    subprocess.run(command, check=True)


def main() -> None:
    cargo_home = Path(os.environ["CARGO_HOME"])
    rustup_home = Path(os.environ["RUSTUP_HOME"])
    soldr_root = Path(os.environ["SOLDR_CACHE_DIR"])
    bin_dir = Path(cargo_home / "bin")

    for path in (cargo_home, rustup_home, soldr_root, soldr_root / "cache", soldr_root / "bin", bin_dir):
        path.mkdir(parents=True, exist_ok=True)

    rustup = shutil.which("rustup")
    if rustup is None:
        sys.exit(
            "setup-soldr requires rustup to already be available on the runner. "
            "GitHub-hosted runners include rustup; self-hosted runners must provide it."
        )

    channel = os.environ.get("SETUP_SOLDR_TOOLCHAIN_CHANNEL", "").strip() or "stable"
    profile = os.environ.get("SETUP_SOLDR_TOOLCHAIN_PROFILE", "").strip() or "minimal"
    components = json.loads(os.environ.get("SETUP_SOLDR_TOOLCHAIN_COMPONENTS", "[]"))
    targets = json.loads(os.environ.get("SETUP_SOLDR_TOOLCHAIN_TARGETS", "[]"))

    run([rustup, "set", "profile", profile])
    run([rustup, "toolchain", "install", channel, "--profile", profile])
    if components:
        run([rustup, "component", "add", "--toolchain", channel, *components])
    if targets:
        run([rustup, "target", "add", "--toolchain", channel, *targets])
    run([rustup, "default", channel])

    cargo = shutil.which("cargo")
    rustc = shutil.which("rustc")
    if cargo is None or rustc is None:
        sys.exit(
            "setup-soldr failed to expose cargo/rustc after rustup configured the toolchain"
        )

    run([cargo, "--version"])
    run([rustc, "--version"])

    output = os.environ.get("GITHUB_OUTPUT")
    if output:
        with open(output, "a", encoding="utf-8") as fh:
            fh.write(f"toolchain={channel}\n")
