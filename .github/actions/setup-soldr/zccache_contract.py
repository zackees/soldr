#!/usr/bin/env python3
from __future__ import annotations

import hashlib
import json
from pathlib import Path
from typing import Any

REPO_ROOT = Path(__file__).resolve().parents[3]
CONTRACT_PATH = REPO_ROOT / "contracts" / "zccache-runtime.v1.json"


def load_contract(path: Path = CONTRACT_PATH) -> dict[str, Any]:
    return json.loads(path.read_text(encoding="utf-8"))


CONTRACT = load_contract()
ARCHIVE_EXT = str(CONTRACT["release_archive"]["extension"])
MANIFEST_NAME = str(CONTRACT["release_archive"]["manifest_name"])
MANIFEST_MIN_SCHEMA_VERSION = int(
    CONTRACT["release_archive"]["manifest_min_schema_version"]
)
ZCCACHE_BUNDLED_BINARIES = tuple(
    CONTRACT.get("zccache", {}).get("required_binaries", ())
)
CRGX_BUNDLED_BINARY = str(CONTRACT["crgx"]["required_binaries"][0])
CARGO_CHEF_BUNDLED_BINARY = str(CONTRACT["cargo_chef"]["required_binaries"][0])
RELEASE_BUNDLED_BINARIES = tuple(CONTRACT["release_archive"]["required_binaries"])
CRGX_LOCAL_DIR_ENV = str(CONTRACT["crgx"]["local_dir_env"])
CARGO_CHEF_LOCAL_DIR_ENV = str(CONTRACT["cargo_chef"]["local_dir_env"])


def binary_name(base: str, *, windows: bool) -> str:
    return f"{base}.exe" if windows else base


def release_binary_names(*, windows: bool) -> tuple[str, ...]:
    return tuple(
        binary_name(base, windows=windows) for base in RELEASE_BUNDLED_BINARIES
    )


def zccache_target_for_soldr_target(soldr_target: str) -> str:
    if bool(CONTRACT.get("zccache", {}).get("embedded")):
        return soldr_target
    if "-unknown-linux-" not in soldr_target:
        return soldr_target
    arch, _, _ = soldr_target.partition("-unknown-linux-")
    libc = CONTRACT["release_archive"]["linux_zccache_target_libc"]
    return f"{arch}-unknown-linux-{libc}"


def _sha256_file(path: Path) -> str:
    hasher = hashlib.sha256()
    with path.open("rb") as fh:
        for chunk in iter(lambda: fh.read(1024 * 1024), b""):
            hasher.update(chunk)
    return hasher.hexdigest()


def _manifest_binaries(manifest: dict[str, Any]) -> dict[str, str]:
    binaries: dict[str, str] = {}
    soldr = manifest.get("soldr", {})
    if isinstance(soldr, dict):
        binary = soldr.get("binary")
        sha = soldr.get("sha256")
        if isinstance(binary, str) and isinstance(sha, str):
            binaries[binary] = sha
        sidecars = soldr.get("sidecars", [])
        if isinstance(sidecars, list):
            for entry in sidecars:
                if not isinstance(entry, dict):
                    continue
                name = entry.get("name")
                sha = entry.get("sha256")
                if isinstance(name, str) and isinstance(sha, str):
                    binaries[name] = sha
    zccache = manifest.get("zccache", {})
    if isinstance(zccache, dict):
        for entry in zccache.get("binaries", []):
            if not isinstance(entry, dict):
                continue
            name = entry.get("name")
            sha = entry.get("sha256")
            if isinstance(name, str) and isinstance(sha, str):
                binaries[name] = sha
    crgx = manifest.get("crgx", {})
    if isinstance(crgx, dict):
        binary = crgx.get("binary")
        sha = crgx.get("sha256")
        if isinstance(binary, str) and isinstance(sha, str):
            binaries[binary] = sha
    cargo_chef = manifest.get("cargo_chef", {})
    if isinstance(cargo_chef, dict):
        binary = cargo_chef.get("binary")
        sha = cargo_chef.get("sha256")
        if isinstance(binary, str) and isinstance(sha, str):
            binaries[binary] = sha
    return binaries


def soldr_debug_info_entries(manifest: dict[str, Any]) -> list[dict[str, Any]]:
    soldr = manifest.get("soldr", {})
    if not isinstance(soldr, dict):
        return []
    entries = soldr.get("debug_info", [])
    if not isinstance(entries, list):
        return []
    return [
        entry
        for entry in entries
        if isinstance(entry, dict)
        and isinstance(entry.get("name"), str)
        and isinstance(entry.get("sha256"), str)
    ]


def validate_release_manifest(
    manifest: dict[str, Any],
    *,
    soldr_target: str,
    windows: bool,
    extract_dir: Path,
) -> None:
    schema_version = manifest.get("schema_version")
    if (
        not isinstance(schema_version, int)
        or schema_version < MANIFEST_MIN_SCHEMA_VERSION
    ):
        raise RuntimeError(
            f"release manifest schema_version must be >= {MANIFEST_MIN_SCHEMA_VERSION}"
        )
    archive = manifest.get("archive")
    if not isinstance(archive, dict) or archive.get("format") != ARCHIVE_EXT:
        raise RuntimeError(f"release manifest archive.format must be {ARCHIVE_EXT}")
    soldr = manifest.get("soldr")
    if not isinstance(soldr, dict) or soldr.get("target") != soldr_target:
        raise RuntimeError(f"release manifest soldr.target must be {soldr_target}")
    zccache = manifest.get("zccache")
    expected_zccache_target = zccache_target_for_soldr_target(soldr_target)
    if (
        not isinstance(zccache, dict)
        or zccache.get("target") != expected_zccache_target
    ):
        raise RuntimeError(
            f"release manifest zccache.target must be {expected_zccache_target}"
        )
    crgx = manifest.get("crgx")
    if not isinstance(crgx, dict) or crgx.get("target") != soldr_target:
        raise RuntimeError(f"release manifest crgx.target must be {soldr_target}")
    cargo_chef = manifest.get("cargo_chef")
    if not isinstance(cargo_chef, dict) or cargo_chef.get("target") != soldr_target:
        raise RuntimeError(f"release manifest cargo_chef.target must be {soldr_target}")

    expected_names = set(release_binary_names(windows=windows))
    manifest_binaries = _manifest_binaries(manifest)
    missing = sorted(expected_names.difference(manifest_binaries))
    if missing:
        raise RuntimeError(
            f"release manifest is missing bundled binary records: {', '.join(missing)}"
        )

    for name in sorted(expected_names):
        expected_sha = manifest_binaries[name].lower()
        if len(expected_sha) != 64 or any(
            ch not in "0123456789abcdef" for ch in expected_sha
        ):
            raise RuntimeError(
                f"release manifest sha256 for {name} is not lowercase hex"
            )
        binary_path = locate_extracted_file(extract_dir, name)
        actual_sha = _sha256_file(binary_path)
        if actual_sha != expected_sha:
            raise RuntimeError(
                f"release manifest sha256 mismatch for {name}: expected {expected_sha}, got {actual_sha}"
            )

    debug_info = soldr_debug_info_entries(manifest)
    if windows and not debug_info:
        raise RuntimeError("release manifest is missing soldr debug_info PDB entry")
    for entry in debug_info:
        name = str(entry["name"])
        if entry.get("format") != "pdb":
            raise RuntimeError(
                f"unsupported soldr debug_info format for {name}: {entry.get('format')}"
            )
        if not name.lower().endswith(".pdb"):
            raise RuntimeError(f"soldr debug_info entry must name a .pdb file: {name}")
        expected_sha = str(entry["sha256"]).lower()
        if len(expected_sha) != 64 or any(
            ch not in "0123456789abcdef" for ch in expected_sha
        ):
            raise RuntimeError(
                f"release manifest sha256 for {name} is not lowercase hex"
            )
        actual_sha = _sha256_file(locate_extracted_file(extract_dir, name))
        if actual_sha != expected_sha:
            raise RuntimeError(
                f"release manifest sha256 mismatch for {name}: expected {expected_sha}, got {actual_sha}"
            )


def locate_extracted_file(root: Path, file_name: str) -> Path:
    for candidate in root.rglob(file_name):
        if candidate.is_file():
            return candidate
    raise RuntimeError(f"downloaded archive did not contain {file_name}")
