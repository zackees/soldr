#!/usr/bin/env python3
from __future__ import annotations

import json
import os
import platform
import shutil
import stat
import subprocess
import sys
import tempfile
import urllib.error
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from zccache_contract import (  # noqa: E402
    ARCHIVE_EXT,
    CARGO_CHEF_BUNDLED_BINARY,
    CARGO_CHEF_LOCAL_DIR_ENV,
    CRGX_LOCAL_DIR_ENV,
    CRGX_BUNDLED_BINARY,
    MANIFEST_NAME,
    ZCCACHE_BUNDLED_BINARIES,
    ZCCACHE_LOCAL_DIR_ENV,
    locate_extracted_file,
    validate_release_manifest,
)


def _normalize_version(value: str) -> str:
    return value[1:] if value.startswith("v") else value


def _detect_target() -> tuple[str, str]:
    machine = platform.machine().lower()
    if machine in {"x86_64", "amd64"}:
        arch = "x86_64"
    elif machine in {"arm64", "aarch64"}:
        arch = "aarch64"
    else:
        raise RuntimeError(f"unsupported architecture: {machine}")

    system = platform.system()
    if system == "Linux":
        # Linux gnu variant runs everywhere this action runs (the
        # GitHub-hosted Linux runners are glibc). musl variant exists
        # too but isn't needed here.
        return f"{arch}-unknown-linux-gnu", "soldr"
    if system == "Darwin":
        return f"{arch}-apple-darwin", "soldr"
    if system == "Windows":
        return f"{arch}-pc-windows-msvc", "soldr.exe"

    raise RuntimeError(f"unsupported operating system: {system}")


def _release_url(repo: str, version: str) -> str:
    if version:
        tag = version if version.startswith("v") else f"v{version}"
        return f"https://api.github.com/repos/{repo}/releases/tags/{tag}"
    return f"https://api.github.com/repos/{repo}/releases/latest"


def _auth_token_for_repo(repo: str) -> str:
    explicit = os.environ.get("SETUP_SOLDR_GITHUB_TOKEN", "").strip()
    if explicit:
        return explicit

    token = os.environ.get("GITHUB_TOKEN", "").strip()
    if not token:
        return ""

    current_repo = os.environ.get("GITHUB_REPOSITORY", "").strip().lower()
    if current_repo == repo.strip().lower():
        return token
    return ""


def _request_headers(repo: str) -> dict[str, str]:
    headers = {
        "Accept": "application/vnd.github+json",
        "X-GitHub-Api-Version": "2022-11-28",
        "User-Agent": "setup-soldr-action",
    }
    token = _auth_token_for_repo(repo)
    if token:
        headers["Authorization"] = f"Bearer {token}"
    return headers


def _fetch_release(repo: str, version: str) -> dict[str, object]:
    request = urllib.request.Request(
        _release_url(repo, version),
        headers=_request_headers(repo),
    )
    with urllib.request.urlopen(request) as response:
        return json.load(response)


def _installed_version(binary_path: Path) -> str | None:
    if not binary_path.exists():
        return None

    output = subprocess.check_output([str(binary_path), "version", "--json"], text=True)
    payload = json.loads(output)
    return str(payload["soldr_version"])


def _select_asset(release: dict[str, object], target: str) -> tuple[str, str]:
    assets = release.get("assets") or []
    suffix = f"-{target}.{ARCHIVE_EXT}"
    for asset in assets:
        if not isinstance(asset, dict):
            continue
        name = str(asset.get("name", ""))
        if name.endswith(suffix):
            return name, str(asset["browser_download_url"])
    raise RuntimeError(f"no release asset found for target {target} (looking for *{suffix})")


def _extract_archive(archive_path: Path, out_dir: Path) -> None:
    """Extract a .tar.zst archive using the system tar's --zstd flag.

    Both GNU tar 1.31+ (Linux) and bsdtar (default on macOS / Windows)
    speak --zstd; the alternative `--use-compress-program=unzstd`
    path covers older GNU tar versions that may linger on long-lived
    runners. Last-resort fallback decompresses with the zstd CLI to a
    sibling .tar then untars that.
    """

    out_dir.mkdir(parents=True, exist_ok=True)
    attempts = (
        ["tar", "--zstd", "-xf", str(archive_path), "-C", str(out_dir)],
        ["tar", "--use-compress-program=unzstd", "-xf", str(archive_path), "-C", str(out_dir)],
    )
    for cmd in attempts:
        result = subprocess.run(cmd, check=False)
        if result.returncode == 0:
            return
    intermediate = archive_path.with_suffix(archive_path.suffix + ".tar")
    subprocess.run(["zstd", "-d", "-o", str(intermediate), str(archive_path)], check=True)
    subprocess.run(["tar", "-xf", str(intermediate), "-C", str(out_dir)], check=True)
    try:
        intermediate.unlink()
    except OSError:
        pass


def _locate_binary(out_dir: Path, binary_name: str) -> Path:
    return locate_extracted_file(out_dir, binary_name)


def _bundled_files_present(install_dir: Path, bases: tuple[str, ...]) -> bool:
    binary_ext = ".exe" if os.name == "nt" else ""
    return all((install_dir / f"{base}{binary_ext}").is_file() for base in bases)


def _export_bundle_env(install_dir: Path) -> None:
    github_env = os.environ.get("GITHUB_ENV")
    if not github_env:
        return
    with open(github_env, "a", encoding="utf-8") as fh:
        if not os.environ.get(ZCCACHE_LOCAL_DIR_ENV) and _bundled_files_present(
            install_dir,
            ZCCACHE_BUNDLED_BINARIES,
        ):
            fh.write(f"{ZCCACHE_LOCAL_DIR_ENV}={install_dir}\n")
        if not os.environ.get(CRGX_LOCAL_DIR_ENV) and _bundled_files_present(
            install_dir,
            (CRGX_BUNDLED_BINARY,),
        ):
            fh.write(f"{CRGX_LOCAL_DIR_ENV}={install_dir}\n")
        if not os.environ.get(CARGO_CHEF_LOCAL_DIR_ENV) and _bundled_files_present(
            install_dir,
            (CARGO_CHEF_BUNDLED_BINARY,),
        ):
            fh.write(f"{CARGO_CHEF_LOCAL_DIR_ENV}={install_dir}\n")


def main() -> None:
    install_dir = Path(os.environ["SOLDR_INSTALL_DIR"])
    install_dir.mkdir(parents=True, exist_ok=True)
    binary_name = "soldr.exe" if os.name == "nt" else "soldr"
    binary_path = install_dir / binary_name
    requested_version = os.environ.get("SETUP_SOLDR_VERSION", "").strip()

    current = _installed_version(binary_path)
    if current is not None:
        if not requested_version or _normalize_version(current) == _normalize_version(
            requested_version
        ):
            _export_bundle_env(install_dir)
            output = os.environ.get("GITHUB_OUTPUT")
            if output:
                with open(output, "a", encoding="utf-8") as fh:
                    fh.write(f"installed_version={current}\n")
            return

    repo = os.environ.get("SOLDR_REPO", "zackees/soldr").strip() or "zackees/soldr"
    target, binary_name = _detect_target()
    release = _fetch_release(repo, requested_version)
    asset_name, download_url = _select_asset(release, target)
    tag_name = str(release["tag_name"])
    binary_ext = ".exe" if os.name == "nt" else ""

    with tempfile.TemporaryDirectory() as tmp:
        tmp_dir = Path(tmp)
        archive_path = tmp_dir / asset_name
        extract_dir = tmp_dir / "extract"
        urllib.request.urlretrieve(download_url, archive_path)
        _extract_archive(archive_path, extract_dir)

        manifest_path = _locate_binary(extract_dir, MANIFEST_NAME)
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        validate_release_manifest(
            manifest,
            soldr_target=target,
            windows=os.name == "nt",
            extract_dir=extract_dir,
        )

        # Stage soldr.
        source = _locate_binary(extract_dir, binary_name)
        shutil.copy2(source, binary_path)
        if os.name != "nt":
            binary_path.chmod(
                binary_path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH
            )

        # Stage the bundled zccache trio next to soldr so the install
        # dir works as a self-contained SOLDR_ZCCACHE_LOCAL_DIR.
        for base in ZCCACHE_BUNDLED_BINARIES:
            file_name = f"{base}{binary_ext}"
            zccache_src = _locate_binary(extract_dir, file_name)
            zccache_dst = install_dir / file_name
            shutil.copy2(zccache_src, zccache_dst)
            if os.name != "nt":
                zccache_dst.chmod(
                    zccache_dst.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH,
                )

        # Stage the bundled crgx next to soldr so the install dir
        # also doubles as SOLDR_CRGX_LOCAL_DIR for `soldr crgx ...`.
        crgx_file_name = f"{CRGX_BUNDLED_BINARY}{binary_ext}"
        crgx_src = _locate_binary(extract_dir, crgx_file_name)
        crgx_dst = install_dir / crgx_file_name
        shutil.copy2(crgx_src, crgx_dst)
        if os.name != "nt":
            crgx_dst.chmod(
                crgx_dst.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH,
            )

        # Stage bundled cargo-chef next to soldr so the install dir also
        # doubles as SOLDR_CARGO_CHEF_LOCAL_DIR for `soldr cook`.
        cargo_chef_file_name = f"{CARGO_CHEF_BUNDLED_BINARY}{binary_ext}"
        cargo_chef_src = _locate_binary(extract_dir, cargo_chef_file_name)
        cargo_chef_dst = install_dir / cargo_chef_file_name
        shutil.copy2(cargo_chef_src, cargo_chef_dst)
        if os.name != "nt":
            cargo_chef_dst.chmod(
                cargo_chef_dst.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH,
            )

        shutil.copy2(manifest_path, install_dir / MANIFEST_NAME)

    _export_bundle_env(install_dir)

    output = os.environ.get("GITHUB_OUTPUT")
    if output:
        with open(output, "a", encoding="utf-8") as fh:
            fh.write(f"installed_version={tag_name}\n")


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, urllib.error.URLError, subprocess.CalledProcessError) as exc:
        sys.exit(str(exc))
