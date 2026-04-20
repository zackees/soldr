#!/usr/bin/env python3
from __future__ import annotations

import json
import os
import platform
import stat
import shutil
import subprocess
import sys
from pathlib import Path
from urllib.error import URLError
from urllib.request import urlopen


def run(command: list[str]) -> None:
    subprocess.run(command, check=True)


def append_github_env(name: str, value: str) -> None:
    output = os.environ.get("GITHUB_ENV")
    if not output:
        return
    with open(output, "a", encoding="utf-8") as fh:
        fh.write(f"{name}={value}\n")


def rustup_init_target_triple() -> str:
    system = platform.system().lower()
    machine = platform.machine().lower()

    if system == "windows":
        if machine in {"amd64", "x86_64"}:
            return "x86_64-pc-windows-msvc"
        if machine in {"arm64", "aarch64"}:
            return "aarch64-pc-windows-msvc"
        if machine in {"x86", "i386", "i686"}:
            return "i686-pc-windows-msvc"
    elif system == "darwin":
        if machine in {"arm64", "aarch64"}:
            return "aarch64-apple-darwin"
        if machine in {"amd64", "x86_64"}:
            return "x86_64-apple-darwin"
    elif system == "linux":
        if machine in {"amd64", "x86_64"}:
            return "x86_64-unknown-linux-gnu"
        if machine in {"arm64", "aarch64"}:
            return "aarch64-unknown-linux-gnu"
        if machine in {"x86", "i386", "i686"}:
            return "i686-unknown-linux-gnu"

    raise RuntimeError(f"unsupported platform for rustup bootstrap: {system}/{machine}")


def rustup_init_url() -> str:
    target = rustup_init_target_triple()
    suffix = ".exe" if target.endswith("windows-msvc") else ""
    return f"https://static.rust-lang.org/rustup/dist/{target}/rustup-init{suffix}"


def download_rustup_init(destination_dir: Path) -> Path:
    filename = "rustup-init.exe" if os.name == "nt" else "rustup-init"
    destination = destination_dir / filename
    if destination.exists():
        return destination

    url = rustup_init_url()
    temp_destination = destination.with_name(f"{destination.name}.tmp")
    try:
        with urlopen(url) as response, open(temp_destination, "wb") as fh:
            shutil.copyfileobj(response, fh)
        temp_destination.replace(destination)
    except (OSError, URLError) as exc:
        if temp_destination.exists():
            temp_destination.unlink()
        raise RuntimeError(f"setup-soldr failed to download rustup-init from {url}: {exc}") from exc

    if os.name != "nt":
        destination.chmod(destination.stat().st_mode | stat.S_IEXEC)

    return destination


def ensure_rustup_available(soldr_root: Path) -> str:
    rustup = shutil.which("rustup")
    if rustup is not None:
        return rustup

    installer_dir = soldr_root / "cache"
    installer_dir.mkdir(parents=True, exist_ok=True)
    installer = download_rustup_init(installer_dir)
    run([str(installer), "-y", "--no-modify-path", "--default-toolchain", "none"])

    rustup = shutil.which("rustup")
    if rustup is None:
        sys.exit("setup-soldr failed to bootstrap rustup on the runner")

    return rustup


def main() -> None:
    cargo_home = Path(os.environ["CARGO_HOME"])
    rustup_home = Path(os.environ["RUSTUP_HOME"])
    soldr_root = Path(os.environ["SOLDR_CACHE_DIR"])
    bin_dir = Path(cargo_home / "bin")

    for path in (cargo_home, rustup_home, soldr_root, soldr_root / "cache", soldr_root / "bin", bin_dir):
        path.mkdir(parents=True, exist_ok=True)

    rustup = ensure_rustup_available(soldr_root)

    channel = os.environ.get("SETUP_SOLDR_TOOLCHAIN_CHANNEL", "").strip() or "stable"
    profile = os.environ.get("SETUP_SOLDR_TOOLCHAIN_PROFILE", "").strip() or "minimal"
    components = json.loads(os.environ.get("SETUP_SOLDR_TOOLCHAIN_COMPONENTS", "[]"))
    targets = json.loads(os.environ.get("SETUP_SOLDR_TOOLCHAIN_TARGETS", "[]"))

    run([rustup, "set", "profile", profile])
    install_command = [rustup, "toolchain", "install", channel, "--profile", profile]
    for component in components:
        install_command.extend(["--component", component])
    for target in targets:
        install_command.extend(["--target", target])
    run(install_command)
    run([rustup, "default", channel])
    os.environ["RUSTUP_TOOLCHAIN"] = channel
    append_github_env("RUSTUP_TOOLCHAIN", channel)

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
