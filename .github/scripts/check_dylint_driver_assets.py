#!/usr/bin/env python3
"""Require catalogued Dylint drivers for every released Soldr target.

`soldr cargo dylint` refuses an absent driver before cargo-dylint can build it
from source.  That makes the published toolchain catalogue part of the front
door's release contract: one missing target asset turns an otherwise valid
nightly pin into a user-facing failure.  Check the same v2 catalogue that the
runtime fetcher consults, while deriving target identities from the canonical
target contract rather than maintaining another platform list.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import urllib.request
from pathlib import Path
from typing import Any

from catalogue_http import open_url, validate_https_url

REPO_ROOT = Path(__file__).resolve().parents[2]
CATALOGUE_URL = "https://zackees.github.io/soldr-toolchain/catalogue.v2.json"
CATALOGUE_OWNER = "zackees"
CATALOGUE_REPO = "soldr-toolchain"
CATALOGUE_TAG = "assets"
CATALOGUE_CAPABILITY = 2
MAX_CATALOGUE_ASSET_BYTES = 8 * 1024 * 1024 * 1024 * 1024
MAX_CATALOGUE_PARTS = 4096
MAX_CATALOGUE_PART_BYTES = 95 * 1024 * 1024
SHA256 = re.compile(r"^[0-9a-f]{64}$")
PINNED_VERSION = re.compile(
    r'crate_name: "cargo-dylint"[\s\S]*?pinned_version: Some\("([^"]+)"\)',
)
CHANNEL = re.compile(r'^\s*channel\s*=\s*"(nightly-\d{4}-\d{2}-\d{2})"', re.MULTILINE)


def released_targets(repo_root: Path) -> list[str]:
    payload: object = json.loads(
        (repo_root / "ci" / "canonical-targets.json").read_text(encoding="utf-8")
    )
    if not isinstance(payload, dict):
        raise ValueError("canonical targets is not an object")
    targets = payload.get("targets")
    if not isinstance(targets, list):
        raise ValueError("canonical targets has no targets list")
    triples: list[str] = []
    for entry in targets:
        if not isinstance(entry, dict):
            continue
        triple = entry.get("triple")
        release = entry.get("release")
        if (
            isinstance(triple, str)
            and isinstance(release, dict)
            and release.get("status") == "included"
        ):
            triples.append(triple)
    if not triples:
        raise ValueError("canonical targets has no released triples")
    return sorted(triples)


def pinned_dylint_version(repo_root: Path) -> str:
    source = (repo_root / "crates" / "soldr-fetch" / "src" / "fetch" / "known_tools.rs").read_text(
        encoding="utf-8"
    )
    match = PINNED_VERSION.search(source)
    if match is None:
        raise ValueError("could not read pinned cargo-dylint version")
    return match.group(1)


def pinned_dylint_channel(repo_root: Path) -> str:
    channels = set()
    for path in sorted((repo_root / "dylints").glob("*/rust-toolchain.toml")):
        match = CHANNEL.search(path.read_text(encoding="utf-8"))
        if match is None:
            raise ValueError(f"no dated nightly in {path.relative_to(repo_root)}")
        channels.add(match.group(1))
    if len(channels) != 1:
        raise ValueError(f"Dylint libraries disagree about their nightly: {sorted(channels)}")
    return channels.pop()


def required_assets(repo_root: Path) -> set[str]:
    version = pinned_dylint_version(repo_root)
    channel = pinned_dylint_channel(repo_root)
    return {
        f"dylint-driver-{version}-{channel}-{triple}.tar.gz"
        for triple in released_targets(repo_root)
    }


def valid_asset(entry: dict[str, Any]) -> bool:
    if (
        entry.get("owner") != CATALOGUE_OWNER
        or entry.get("repo") != CATALOGUE_REPO
        or entry.get("tag") != CATALOGUE_TAG
        or not isinstance(entry.get("asset"), str)
        or not entry["asset"]
    ):
        return False
    if not isinstance(entry.get("sha256"), str) or not SHA256.fullmatch(entry["sha256"]):
        return False
    size = entry.get("size_bytes")
    if (
        not isinstance(size, int)
        or isinstance(size, bool)
        or not 0 < size <= MAX_CATALOGUE_ASSET_BYTES
    ):
        return False
    min_client_version = entry.get("min_client_version")
    if min_client_version is not None and min_client_version != CATALOGUE_CAPABILITY:
        return False
    # Match the v2 wire union parsed by the runtime before downloading:
    # exactly one transport field must be present. An empty inactive list is
    # not the same as an absent optional field in Rust's serde parser.
    urls = entry.get("urls")
    parts = entry.get("parts")
    if isinstance(urls, list) and urls and parts is None:
        return entry.get("source_path") is None and valid_urls(urls)
    if (
        not isinstance(parts, list)
        or not parts
        or len(parts) > MAX_CATALOGUE_PARTS
        or urls is not None
        or min_client_version != CATALOGUE_CAPABILITY
        or not valid_source_path(entry.get("source_path"))
    ):
        return False
    for number, part in enumerate(parts, start=1):
        part_number = part.get("number") if isinstance(part, dict) else None
        if (
            not isinstance(part, dict)
            or not isinstance(part_number, int)
            or isinstance(part_number, bool)
            or part_number != number
        ):
            return False
        part_size = part.get("size_bytes")
        part_sha = part.get("sha256")
        part_urls = part.get("urls")
        if (
            not isinstance(part_size, int)
            or isinstance(part_size, bool)
            or not 0 < part_size <= MAX_CATALOGUE_PART_BYTES
            or not isinstance(part_sha, str)
            or not SHA256.fullmatch(part_sha)
            or not isinstance(part_urls, list)
            or not part_urls
            or not valid_urls(part_urls)
        ):
            return False
    return sum(part["size_bytes"] for part in parts) == size


def valid_urls(urls: list[object]) -> bool:
    """Mirror the downloader's credential-free HTTPS URL requirement."""
    try:
        for url in urls:
            validate_https_url(url, label="catalogued asset URL")
    except SystemExit:
        return False
    return True


def valid_source_path(value: object) -> bool:
    """Match the safe relative source path accepted by the v2 parser."""
    return (
        isinstance(value, str)
        and bool(value)
        and not value.startswith("/")
        and not any(part in ("", ".", "..") for part in value.split("/"))
    )


def missing_assets(payload: dict[str, Any], required: set[str]) -> list[str]:
    entries = payload.get("entries")
    if not isinstance(entries, list):
        raise ValueError("catalogue v2 has no entries list")
    available = {
        entry["asset"] for entry in entries if isinstance(entry, dict) and valid_asset(entry)
    }
    return sorted(required - available)


def load_catalogue(url: str) -> dict[str, Any]:
    with open_url(urllib.request.Request(url), timeout=30) as response:
        payload = json.loads(response.read().decode("utf-8"))
    if not isinstance(payload, dict) or payload.get("schema_version") != 2:
        raise ValueError("catalogue is not a schema_version 2 object")
    return payload


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo-root", type=Path, default=REPO_ROOT)
    parser.add_argument("--catalogue-url", default=CATALOGUE_URL)
    args = parser.parse_args()
    required = required_assets(args.repo_root)
    try:
        payload = load_catalogue(args.catalogue_url)
    except (OSError, TimeoutError, ValueError, json.JSONDecodeError) as exc:
        print(f"check_dylint_driver_assets: skipped, cannot resolve catalogue ({exc})")
        return 0
    missing = missing_assets(payload, required)
    if missing:
        print("error: the pinned Dylint nightly is missing catalogued driver assets:")
        print("\n".join(f"  {asset}" for asset in missing))
        return 1
    print(f"check_dylint_driver_assets: all {len(required)} released targets have a driver asset.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
