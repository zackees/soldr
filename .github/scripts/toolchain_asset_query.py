#!/usr/bin/env python3
"""Resolve a tool asset URL from the soldr-toolchain public manifests.

The script preserves the friendly call surface the old manifest-branch
query script exposed:

    python3 .github/scripts/toolchain_asset_query.py \
        --platform linux --arch x86 --extra gnu cargo-zigbuild

It reads ``https://zackees.github.io/soldr-toolchain/<tool>/manifest.json``,
selects a release, matches a platform entry, and prints one URL on stdout.
With ``--json`` it emits the selected version/platform/filename/digest
metadata used by deterministic CI packaging.
Exit status is non-zero when the requested tool/version/platform is absent.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any

DEFAULT_ORIGIN = "https://zackees.github.io/soldr-toolchain"

OS_ALIASES: dict[str, str] = {
    "linux": "linux",
    "mac": "darwin",
    "macos": "darwin",
    "darwin": "darwin",
    "windows": "windows",
    "win": "windows",
}

ARCH_ALIASES: dict[str, str] = {
    "x86": "x86_64",
    "x64": "x86_64",
    "amd64": "x86_64",
    "x86_64": "x86_64",
    "arm": "aarch64",
    "arm64": "aarch64",
    "aarch64": "aarch64",
    "universal2": "universal2",
}


def fetch_json(url: str) -> Any:
    try:
        with urllib.request.urlopen(url, timeout=30) as resp:
            return json.loads(resp.read().decode("utf-8"))
    except urllib.error.HTTPError as exc:
        raise SystemExit(f"HTTP {exc.code} fetching {url}") from exc
    except urllib.error.URLError as exc:
        raise SystemExit(f"network error fetching {url}: {exc}") from exc


def tool_manifest_url(origin: str, tool: str) -> str:
    return f"{origin.rstrip('/')}/{tool}/manifest.json"


def normalize_os(value: str) -> str:
    os_key = OS_ALIASES.get(value.lower())
    if os_key is None:
        raise SystemExit(
            f"unknown --platform '{value}'. Accepted: {', '.join(sorted(OS_ALIASES))}"
        )
    return os_key


def normalize_arch(value: str) -> str:
    arch_key = ARCH_ALIASES.get(value.lower())
    if arch_key is None:
        raise SystemExit(
            f"unknown --arch '{value}'. Accepted: {', '.join(sorted(ARCH_ALIASES))}"
        )
    return arch_key


def platform_candidates(
    os_key: str, arch_key: str, extra: str | None
) -> list[dict[str, str]]:
    base = {"os": os_key, "arch": arch_key}
    if extra:
        normalized = extra.lower()
        if os_key == "linux":
            libc = "glibc" if normalized in {"gnu", "glibc"} else normalized
            if libc == "glibc":
                return [base | {"libc": "glibc"}, base | {"libc": "musl"}]
            return [base | {"libc": libc}]
        if os_key == "windows":
            return [base | {"abi": normalized}]
        return [base | {"abi": normalized}]

    if os_key == "linux":
        return [base | {"libc": "glibc"}, base | {"libc": "musl"}, base]
    if os_key == "windows":
        return [
            base | {"abi": "msvc"},
            base | {"abi": "gnu"},
            base | {"abi": "gnullvm"},
            base,
        ]
    if os_key == "darwin" and arch_key != "universal2":
        return [base, {"os": "darwin", "arch": "universal2"}]
    return [base]


def version_forms(value: str) -> set[str]:
    if value in {"", "latest"}:
        return {value}
    bare = value.removeprefix("v")
    return {value, bare, f"v{bare}"}


def find_release(payload: dict[str, Any], requested: str) -> dict[str, Any]:
    releases = payload.get("releases")
    if not isinstance(releases, list) or not releases:
        raise SystemExit("tool manifest has no releases")

    selected = requested
    if requested in {"", "latest"}:
        raw_channels = payload.get("channels")
        channels = raw_channels if isinstance(raw_channels, dict) else {}
        selected = (
            channels.get("latest-stable")
            or channels.get("stable")
            or releases[0].get("version")
            or ""
        )

    accepted = version_forms(str(selected))
    for release in releases:
        if not isinstance(release, dict):
            continue
        version = str(release.get("version", ""))
        if version in accepted:
            return release

    known = ", ".join(
        str(r.get("version", "?")) for r in releases[:8] if isinstance(r, dict)
    )
    raise SystemExit(
        f"no release '{requested}' in tool manifest. Known versions: {known}"
    )


def platform_matches(actual: dict[str, Any], expected: dict[str, str]) -> bool:
    for key in ("os", "arch", "libc", "abi"):
        if key in expected:
            if actual.get(key) != expected[key]:
                return False
        elif key in actual:
            return False
    return True


def find_asset_url(release: dict[str, Any], candidates: list[dict[str, str]]) -> str:
    return str(find_asset(release, candidates, require_sha256=False)["urls"][0])


def find_asset(
    release: dict[str, Any],
    candidates: list[dict[str, str]],
    *,
    require_sha256: bool = True,
) -> dict[str, Any]:
    platforms = release.get("platforms")
    if not isinstance(platforms, list):
        raise SystemExit("release has no platforms list")

    for candidate in candidates:
        for entry in platforms:
            if not isinstance(entry, dict):
                continue
            platform = entry.get("platform")
            asset = entry.get("asset")
            if not isinstance(platform, dict) or not isinstance(asset, dict):
                continue
            if platform_matches(platform, candidate):
                raw_urls = asset.get("urls")
                raw_parts = asset.get("parts")
                # Bound as concrete lists rather than truthiness flags: mypy
                # does not narrow `Any | None` through a separate boolean.
                urls: list[Any] = raw_urls if isinstance(raw_urls, list) else []
                parts: list[Any] = raw_parts if isinstance(raw_parts, list) else []
                has_urls = bool(urls)
                has_parts = bool(parts)
                # soldr#2850's catalogue v2: an asset carries EITHER direct
                # urls OR parts. Requiring `urls` here rejected every
                # multipart row, which today is nearly all of them -- the
                # published cargo-nextest assets have `parts` and no `urls`
                # at all, so this raised "matched asset has no URL" for an
                # asset that was present and resolvable.
                if has_urls or has_parts:
                    digest = str(asset.get("sha256", "")).lower()
                    if require_sha256 and (
                        len(digest) != 64
                        or any(ch not in "0123456789abcdef" for ch in digest)
                    ):
                        raise SystemExit("matched asset has no valid sha256")
                    return {
                        "platform": platform,
                        "filename": asset.get("filename"),
                        "urls": [str(url) for url in urls],
                        "parts": list(parts),
                        "sha256": digest,
                        "size_bytes": asset.get("size_bytes"),
                    }
                raise SystemExit("matched asset has neither URLs nor parts")

    wanted = " or ".join(
        "-".join(
            candidate.get(k, "")
            for k in ("os", "arch", "libc", "abi")
            if candidate.get(k)
        )
        for candidate in candidates
    )
    available = []
    for entry in platforms:
        platform = entry.get("platform") if isinstance(entry, dict) else None
        if isinstance(platform, dict):
            available.append(
                "-".join(
                    str(platform.get(k))
                    for k in ("os", "arch", "libc", "abi")
                    if platform.get(k)
                )
            )
    raise SystemExit(
        f"no platform match for {wanted}; available: {', '.join(sorted(available))}"
    )


def resolve_url(
    *,
    tool: str,
    origin: str,
    tool_manifest_url_override: str | None,
    platform: str,
    arch: str,
    extra: str | None,
    version: str,
) -> str:
    url = tool_manifest_url_override or tool_manifest_url(origin, tool)
    payload = fetch_json(url)
    if not isinstance(payload, dict):
        raise SystemExit(f"tool manifest at {url} is not a JSON object")
    release = find_release(payload, version)
    candidates = platform_candidates(
        normalize_os(platform), normalize_arch(arch), extra
    )
    return find_asset_url(release, candidates)


def resolve_metadata(
    *,
    tool: str,
    origin: str,
    tool_manifest_url_override: str | None,
    platform: str,
    arch: str,
    extra: str | None,
    version: str,
) -> dict[str, Any]:
    url = tool_manifest_url_override or tool_manifest_url(origin, tool)
    payload = fetch_json(url)
    if not isinstance(payload, dict):
        raise SystemExit(f"tool manifest at {url} is not a JSON object")
    release = find_release(payload, version)
    os_key = normalize_os(platform)
    arch_key = normalize_arch(arch)
    asset = find_asset(
        release, platform_candidates(os_key, arch_key, extra), require_sha256=True
    )
    return {
        "schema_version": 1,
        "tool": tool,
        "version": release.get("version"),
        "platform": asset["platform"],
        "filename": asset["filename"],
        "urls": asset["urls"],
        "parts": asset.get("parts", []),
        "sha256": asset["sha256"],
        "size_bytes": asset.get("size_bytes"),
        "manifest_url": url,
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tool", help="Tool name under the soldr-toolchain origin.")
    parser.add_argument(
        "--origin",
        default=DEFAULT_ORIGIN,
        help=f"Catalogue origin (default: {DEFAULT_ORIGIN})",
    )
    parser.add_argument(
        "--tool-manifest-url",
        default=None,
        help="Full per-tool manifest URL override, mainly for tests and mirrors.",
    )
    parser.add_argument(
        "--platform", required=True, help="OS: linux, mac/darwin, windows."
    )
    parser.add_argument(
        "--arch",
        required=True,
        help="Arch: x86/x64/x86_64, arm/arm64/aarch64, universal2.",
    )
    parser.add_argument(
        "--extra", default=None, help="ABI/libc extra: gnu, musl, msvc, gnullvm."
    )
    parser.add_argument(
        "--version", default="latest", help="Release version or latest (default)."
    )
    parser.add_argument(
        "--json", action="store_true", help="emit selected asset metadata as JSON"
    )
    args = parser.parse_args(argv)

    kwargs = {
        "tool": args.tool,
        "origin": args.origin,
        "tool_manifest_url_override": args.tool_manifest_url,
        "platform": args.platform,
        "arch": args.arch,
        "extra": args.extra,
        "version": args.version,
    }
    if args.json:
        print(json.dumps(resolve_metadata(**kwargs), sort_keys=True))
    else:
        print(resolve_url(**kwargs))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())


# --------------------------- asset materialization ---------------------------
#
# soldr#2850's catalogue v2 publishes most assets as ordered parts rather than
# a single URL. Reconstructing them is the same job for every consumer, so it
# lives here rather than being copied into each script -- there are three
# (`fetch_catalogued_nextest`, `download_catalogued_asset`,
# `install_catalogued_tools`) and three copies would drift.

MAX_PART_BYTES = 64 * 1024 * 1024


def _read_all(url: str, *, timeout: int = 120) -> bytes:
    request = urllib.request.Request(url, headers={"Accept-Encoding": "identity"})
    with urllib.request.urlopen(request, timeout=timeout) as response:
        return bytes(response.read())


def write_multipart_asset(parts: list[dict[str, Any]], destination: Path) -> None:
    """Reassemble an ordered multipart asset, verifying each part as it lands.

    Per-part verification is not redundant with the caller's whole-file check:
    it names the part that is wrong, where a whole-file mismatch alone says
    only that something in a multi-hundred-megabyte reassembly is off.
    """
    with destination.open("wb") as handle:
        for expected_number, part in enumerate(parts, start=1):
            number = part.get("number")
            if not isinstance(number, int) or isinstance(number, bool):
                raise SystemExit("multipart asset has a non-integer part number")
            if number != expected_number:
                raise SystemExit(
                    "multipart asset has non-contiguous parts: "
                    f"expected {expected_number}, found {number}"
                )
            size = part.get("size_bytes")
            if (
                not isinstance(size, int)
                or isinstance(size, bool)
                or not 1 <= size <= MAX_PART_BYTES
            ):
                raise SystemExit(
                    f"multipart part {expected_number} has invalid size_bytes {size!r}"
                )
            digest = str(part.get("sha256", "")).lower()
            if len(digest) != 64 or any(ch not in "0123456789abcdef" for ch in digest):
                raise SystemExit(
                    f"multipart part {expected_number} has no valid sha256"
                )
            urls = part.get("urls")
            if not isinstance(urls, list) or not urls:
                raise SystemExit(f"multipart part {expected_number} has no URLs")
            payload = _read_all(str(urls[0]))
            if len(payload) != size:
                raise SystemExit(
                    f"multipart part {expected_number} is {len(payload)} bytes, "
                    f"the manifest says {size}"
                )
            actual = hashlib.sha256(payload).hexdigest()
            if actual != digest:
                raise SystemExit(
                    f"multipart part {expected_number} sha256 mismatch: "
                    f"expected {digest}, got {actual}"
                )
            handle.write(payload)
