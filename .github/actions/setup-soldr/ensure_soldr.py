#!/usr/bin/env python3
from __future__ import annotations

import json
import os
import platform
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import urllib.error
import urllib.request
import zipfile
from pathlib import Path


def _normalize_version(value: str) -> str:
    return value[1:] if value.startswith("v") else value


def _detect_target() -> tuple[str, str, str]:
    machine = platform.machine().lower()
    if machine in {"x86_64", "amd64"}:
        arch = "x86_64"
    elif machine in {"arm64", "aarch64"}:
        arch = "aarch64"
    else:
        raise RuntimeError(f"unsupported architecture: {machine}")

    system = platform.system()
    if system == "Linux":
        return f"{arch}-unknown-linux-gnu", "tar.gz", "soldr"
    if system == "Darwin":
        return f"{arch}-apple-darwin", "tar.gz", "soldr"
    if system == "Windows":
        return f"{arch}-pc-windows-msvc", "zip", "soldr.exe"

    raise RuntimeError(f"unsupported operating system: {system}")


def _release_url(repo: str, version: str) -> str:
    if version:
        tag = version if version.startswith("v") else f"v{version}"
        return f"https://api.github.com/repos/{repo}/releases/tags/{tag}"
    return f"https://api.github.com/repos/{repo}/releases/latest"


def _fetch_release(repo: str, version: str) -> dict[str, object]:
    request = urllib.request.Request(
        _release_url(repo, version),
        headers={
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
            "User-Agent": "setup-soldr-action",
        },
    )
    with urllib.request.urlopen(request) as response:
        return json.load(response)


def _installed_version(binary_path: Path) -> str | None:
    if not binary_path.exists():
        return None

    output = subprocess.check_output([str(binary_path), "version", "--json"], text=True)
    payload = json.loads(output)
    return str(payload["soldr_version"])


def _select_asset(release: dict[str, object], target: str, archive_ext: str) -> tuple[str, str]:
    assets = release.get("assets") or []
    for asset in assets:
        if not isinstance(asset, dict):
            continue
        name = str(asset.get("name", ""))
        if target in name and name.endswith(archive_ext):
            return name, str(asset["browser_download_url"])
    raise RuntimeError(f"no release asset found for target {target}")


def _extract_binary(archive_path: Path, archive_ext: str, binary_name: str, out_dir: Path) -> Path:
    out_dir.mkdir(parents=True, exist_ok=True)
    if archive_ext == "zip":
        with zipfile.ZipFile(archive_path) as archive:
            archive.extractall(out_dir)
    else:
        with tarfile.open(archive_path, "r:gz") as archive:
            archive.extractall(out_dir)

    for candidate in out_dir.rglob(binary_name):
        if candidate.is_file():
            return candidate
    raise RuntimeError(f"downloaded archive did not contain {binary_name}")


def main() -> None:
    install_dir = Path(os.environ["SOLDR_INSTALL_DIR"])
    install_dir.mkdir(parents=True, exist_ok=True)
    binary_name = "soldr.exe" if os.name == "nt" else "soldr"
    binary_path = install_dir / binary_name
    requested_version = os.environ.get("SETUP_SOLDR_VERSION", "").strip()

    current = _installed_version(binary_path)
    if current is not None:
        if not requested_version or _normalize_version(current) == _normalize_version(requested_version):
            output = os.environ.get("GITHUB_OUTPUT")
            if output:
                with open(output, "a", encoding="utf-8") as fh:
                    fh.write(f"installed_version={current}\n")
            return

    repo = os.environ.get("SOLDR_REPO", "zackees/soldr").strip() or "zackees/soldr"
    target, archive_ext, binary_name = _detect_target()
    release = _fetch_release(repo, requested_version)
    asset_name, download_url = _select_asset(release, target, archive_ext)
    tag_name = str(release["tag_name"])

    with tempfile.TemporaryDirectory() as tmp:
        tmp_dir = Path(tmp)
        archive_path = tmp_dir / asset_name
        extract_dir = tmp_dir / "extract"
        urllib.request.urlretrieve(download_url, archive_path)
        source = _extract_binary(archive_path, archive_ext, binary_name, extract_dir)
        shutil.copy2(source, binary_path)
        if os.name != "nt":
            binary_path.chmod(binary_path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)

    output = os.environ.get("GITHUB_OUTPUT")
    if output:
        with open(output, "a", encoding="utf-8") as fh:
            fh.write(f"installed_version={tag_name}\n")


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, urllib.error.URLError, subprocess.CalledProcessError) as exc:
        sys.exit(str(exc))
