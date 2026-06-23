#!/usr/bin/env python3
"""Resolve an asset URL out of soldr's `manifest` branch.

Takes friendly short-form CLI args (`--platform mac`, `--arch arm`) and
maps them onto the manifest's normalized `<os>-<arch>[-<extra>]`
platform keys (`darwin-arm64`, `linux-x64-gnu`, etc.). Prints the
public CDN download URL on stdout — pipe straight into `curl`:

    URL=$(python3 tool_query.py --platform linux --arch x86 --extra gnu cargo-zigbuild)
    curl -fL -o cargo-zigbuild.tar.xz "$URL"

CLI:
    tool_query.py [options] <tool-name>
      --manifest-url URL    Root manifest.json URL. Defaults to soldr's
                            canonical `manifest` branch on github.com.
      --platform OS         windows | mac | linux  (mac → darwin)
      --arch ARCH           x86 | arm | universal2  (x86 → x64; arm → arm64)
      --extra EXTRA         Optional ABI extra (gnu, musl, msvc, gnullvm).
                            When omitted, a per-OS preference order is
                            tried so the most-common variant wins.
      --version VER         Release tag (e.g. `1.12.10`) or `latest`
                            (default).

Output: ONE line, the URL. Exit non-zero if nothing matches.

Example queries against the live manifest:

    python tool_query.py --platform windows --arch x86 --extra msvc cargo-zigbuild
    python tool_query.py --platform mac     --arch arm                zccache
    python tool_query.py --platform linux   --arch x86 --extra musl   cargo-xwin
"""

from __future__ import annotations

import argparse
import json
import urllib.error
import urllib.request
from typing import Any

DEFAULT_MANIFEST_URL = (
    "https://raw.githubusercontent.com/zackees/soldr/manifest/manifest.json"
)

# Friendly short-form OS → manifest's os key.
OS_ALIASES: dict[str, str] = {
    "linux": "linux",
    "mac": "darwin",
    "macos": "darwin",
    "darwin": "darwin",
    "windows": "windows",
    "win": "windows",
}

# Friendly short-form arch → manifest's arch key. The user-facing
# short names are the npm/Node.js style ("x86" for 64-bit Intel,
# "arm" for 64-bit ARM); they expand to the canonical 64-bit keys.
# 32-bit lanes aren't surfaced in the manifest schema at all.
ARCH_ALIASES: dict[str, str] = {
    "x86": "x64",
    "x64": "x64",
    "amd64": "x64",
    "x86_64": "x64",  # accept the rust-triple long form too
    "arm": "arm64",
    "arm64": "arm64",
    "aarch64": "arm64",
    "universal2": "universal2",
}

# When --extra is omitted, fall back through these in order, picking
# the first one present in the release's `platforms` map. The defaults
# match the "MSVC on Windows always" / "glibc on Linux first" norms.
DEFAULT_EXTRA_ORDER: dict[str, list[str | None]] = {
    "linux":   ["gnu", "musl", None],
    "darwin":  [None, "gnu"],
    "windows": ["msvc", "gnu", "gnullvm", None],
}


def fetch_json(url: str) -> Any:
    try:
        with urllib.request.urlopen(url, timeout=30) as resp:
            return json.loads(resp.read().decode("utf-8"))
    except urllib.error.HTTPError as exc:
        raise SystemExit(f"HTTP {exc.code} fetching {url}") from exc
    except urllib.error.URLError as exc:
        raise SystemExit(f"network error fetching {url}: {exc}") from exc


def build_candidate_keys(
    os_key: str, arch_key: str, explicit_extra: str | None
) -> list[str]:
    """Return the candidate `platforms` keys to try, in priority order.

    When `explicit_extra` is set, only that one key is tried. Otherwise
    walk the per-OS default fallback chain so the user gets the
    most-common variant without having to know which extras exist.
    """
    if explicit_extra is not None:
        return [f"{os_key}-{arch_key}-{explicit_extra}"]
    candidates: list[str] = []
    for extra in DEFAULT_EXTRA_ORDER.get(os_key, [None]):
        if extra is None:
            candidates.append(f"{os_key}-{arch_key}")
        else:
            candidates.append(f"{os_key}-{arch_key}-{extra}")
    return candidates


def find_release(per_tool: list[dict[str, Any]], requested: str) -> dict[str, Any]:
    """Pick a release entry from the flat per-tool array.

    `requested == 'latest'` returns the first entry (the array is
    sorted by published_at desc). Otherwise we scan for an entry whose
    ``tag`` or ``version`` matches — some manifests carry a
    ``v``-prefixed tag (``v0.1.0``) and a bare ``version`` field
    (``0.1.0``); accept either form so callers don't have to know
    which convention each tool follows.
    """
    if not per_tool:
        raise SystemExit("per-tool manifest is empty")
    if requested in ("latest", ""):
        return per_tool[0]
    bare = requested.lstrip("v")
    v_prefixed = f"v{bare}"
    for entry in per_tool:
        tag = entry.get("tag")
        version = entry.get("version")
        if tag in (requested, v_prefixed, bare):
            return entry
        if version in (requested, bare):
            return entry
    known = ", ".join(e.get("tag") or "?" for e in per_tool[:6])
    raise SystemExit(
        f"no release '{requested}' in manifest. "
        f"Known tags (newest first): {known}…"
    )


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("tool", help="Tool name as it appears under the root manifest's `tools`.")
    parser.add_argument(
        "--manifest-url", default=DEFAULT_MANIFEST_URL,
        help=f"Root manifest URL (default: {DEFAULT_MANIFEST_URL})",
    )
    parser.add_argument(
        "--platform", required=True,
        help=f"OS: {', '.join(sorted(set(OS_ALIASES)))}.",
    )
    parser.add_argument(
        "--arch", required=True,
        help=f"Arch: {', '.join(sorted(set(ARCH_ALIASES)))}.",
    )
    parser.add_argument(
        "--extra", default=None,
        help=(
            "ABI extra (gnu / musl / msvc / gnullvm / …). Omit to fall "
            "back through a per-OS preference order."
        ),
    )
    parser.add_argument(
        "--version", default="latest",
        help="Release tag (e.g. `1.12.10`) or `latest` (default).",
    )
    args = parser.parse_args()

    os_key = OS_ALIASES.get(args.platform.lower())
    if os_key is None:
        raise SystemExit(
            f"unknown --platform '{args.platform}'. "
            f"Accepted: {', '.join(sorted(set(OS_ALIASES)))}"
        )
    arch_key = ARCH_ALIASES.get(args.arch.lower())
    if arch_key is None:
        raise SystemExit(
            f"unknown --arch '{args.arch}'. "
            f"Accepted: {', '.join(sorted(set(ARCH_ALIASES)))}"
        )

    # Hop 1: the root manifest tells us where each tool's per-tool
    # manifest lives, relative to the same branch.
    root = fetch_json(args.manifest_url)
    tools = root.get("tools") or {}
    tool_entry = tools.get(args.tool)
    if tool_entry is None:
        known = ", ".join(sorted(tools.keys()))
        raise SystemExit(f"tool '{args.tool}' not in manifest. Known: {known}")
    per_tool_path = tool_entry["path"]
    base = args.manifest_url.rsplit("/", 1)[0]
    per_tool_url = f"{base}/{per_tool_path}"

    # Hop 2: per-tool manifest is a flat ARRAY of release dicts,
    # newest-first.
    per_tool = fetch_json(per_tool_url)
    if not isinstance(per_tool, list):
        raise SystemExit(
            f"per-tool manifest at {per_tool_url} is not an array — "
            f"got {type(per_tool).__name__}; manifest branch may need a refresh"
        )
    release = find_release(per_tool, args.version)
    tag = release.get("tag") or args.version
    platforms = release.get("platforms") or {}

    for candidate in build_candidate_keys(os_key, arch_key, args.extra):
        entry = platforms.get(candidate)
        if entry is not None:
            print(entry["url"])
            return 0

    available = ", ".join(sorted(platforms.keys())) or "(none)"
    requested = (
        build_candidate_keys(os_key, arch_key, args.extra)[0]
        if args.extra is not None
        else f"{os_key}-{arch_key}[-...]"
    )
    raise SystemExit(
        f"no platform match for {args.tool} {tag}: wanted '{requested}', "
        f"available: {available}"
    )


if __name__ == "__main__":
    raise SystemExit(main())
