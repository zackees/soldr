#!/usr/bin/env python3
"""Drop tool prebuilts into the ``manifest`` branch.

Reads ``<prebuilts-dir>/*.meta.json`` produced by
``refresh-tool-prebuilts.yml``, copies the sibling tar.zst archive
into ``<manifest-checkout>/deps/<triple>/<tool>/<version>/`` (the path
LFS-tracks via ``.gitattributes``), and patches each per-tool
``<tool>/manifest.json`` so the release entry's ``platforms`` map
points at the LFS-aware CDN URL.

Each ``meta.json`` looks like::

    {
      "tool":      "crgx",
      "version":   "0.1.0",
      "target":    "aarch64-pc-windows-msvc",
      "archive":   "crgx-0.1.0-aarch64-pc-windows-msvc.tar.zst",
      "size":      123456,
      "sha256":    "<64-char lowercase hex>",
      "deps_path": "deps/aarch64-pc-windows-msvc/crgx/0.1.0/crgx-..."
    }

For the manifest.json patching, the (target -> platform-key) mapping
matches what ``tool_query.py`` expects::

    x86_64-pc-windows-msvc   -> windows-x64-msvc
    aarch64-pc-windows-msvc  -> windows-arm64-msvc
    x86_64-unknown-linux-gnu -> linux-x64-gnu
    aarch64-unknown-linux-gnu-> linux-arm64-gnu
    x86_64-unknown-linux-musl-> linux-x64-musl
    aarch64-unknown-linux-musl-> linux-arm64-musl
    x86_64-apple-darwin      -> darwin-x64
    aarch64-apple-darwin     -> darwin-arm64

The URL inserted into each ``platforms`` entry uses the LFS-aware
``media.githubusercontent.com/media/...`` CDN endpoint so consumers
hit the actual binary blob rather than an LFS pointer file.

Idempotent: re-running over the same prebuilts is a no-op. Existing
entries pointing at upstream-released assets (e.g. ``windows-x64-msvc``
for crgx) are preserved unless this script also produces a prebuild
for them — in which case the vendored URL replaces the upstream one
(the vendored prebuilt is the byte we can guarantee shape/version
parity for, since this workflow built it).
"""

from __future__ import annotations

import argparse
import json
import shutil
import sys
from pathlib import Path

# Map rust target triple -> the ``platforms`` key tool_query.py
# resolves to. Mirrors the per-tool manifest format on the manifest
# branch.
TARGET_TO_PLATFORM_KEY: dict[str, str] = {
    "x86_64-pc-windows-msvc":   "windows-x64-msvc",
    "aarch64-pc-windows-msvc":  "windows-arm64-msvc",
    "x86_64-unknown-linux-gnu":  "linux-x64-gnu",
    "aarch64-unknown-linux-gnu": "linux-arm64-gnu",
    "x86_64-unknown-linux-musl": "linux-x64-musl",
    "aarch64-unknown-linux-musl": "linux-arm64-musl",
    "x86_64-apple-darwin":  "darwin-x64",
    "aarch64-apple-darwin": "darwin-arm64",
}

# Soldr's manifest branch lives at zackees/soldr. The /media/ endpoint
# follows Git-LFS pointers; falls back to raw bytes for non-LFS files.
LFS_URL_TEMPLATE = (
    "https://media.githubusercontent.com/media/zackees/soldr/manifest/{rel_path}"
)


def lfs_url_for(deps_path: str) -> str:
    """Construct the public CDN URL for a manifest-branch deps file."""
    return LFS_URL_TEMPLATE.format(rel_path=deps_path)


def patch_per_tool_manifest(
    tool_manifest_path: Path,
    *,
    tool: str,
    version: str,
    platform_key: str,
    archive_name: str,
    url: str,
    size: int,
    sha256: str,
) -> bool:
    """Insert/refresh a single ``platforms`` entry on the matching
    release dict inside ``<tool>/manifest.json``.

    Returns True if the file was modified, False if the entry was
    already byte-identical (no-op).
    """
    if not tool_manifest_path.is_file():
        print(
            f"  warning: per-tool manifest missing: {tool_manifest_path}",
            file=sys.stderr,
        )
        return False
    payload = json.loads(tool_manifest_path.read_text(encoding="utf-8"))
    if not isinstance(payload, list):
        print(
            f"  warning: {tool_manifest_path} is not a flat array; skipping",
            file=sys.stderr,
        )
        return False

    # Find the release entry for the requested version. We accept
    # either tag form (``v0.1.0``) or bare version (``0.1.0``).
    matches: list[dict] = []
    for entry in payload:
        if not isinstance(entry, dict):
            continue
        entry_ver = entry.get("version")
        entry_tag = entry.get("tag")
        if entry_ver == version or entry_tag == f"v{version}" or entry_tag == version:
            matches.append(entry)
    if not matches:
        print(
            f"  warning: no release entry for {tool} version {version} "
            f"in {tool_manifest_path}; skipping platforms patch",
            file=sys.stderr,
        )
        return False

    new_entry = {
        "filename": archive_name,
        "url": url,
        "size": size,
        "sha256": sha256,
        "source": "soldr-prebuilt",
    }
    changed = False
    for entry in matches:
        platforms = entry.setdefault("platforms", {})
        existing = platforms.get(platform_key)
        if existing == new_entry:
            continue
        platforms[platform_key] = new_entry
        changed = True

    if not changed:
        return False
    tool_manifest_path.write_text(
        json.dumps(payload, indent=2, ensure_ascii=False) + "\n",
        encoding="utf-8",
    )
    return True


def install_one(
    meta_path: Path,
    manifest_root: Path,
) -> None:
    """Drop one prebuilt's archive into deps/ and patch its
    per-tool manifest."""
    meta = json.loads(meta_path.read_text(encoding="utf-8"))
    tool = meta["tool"]
    version = meta["version"]
    target = meta["target"]
    archive_name = meta["archive"]
    deps_path = meta["deps_path"]
    sha256 = meta["sha256"]
    size = int(meta["size"])

    archive_src = meta_path.parent / archive_name
    if not archive_src.is_file():
        print(
            f"  error: archive missing alongside meta: {archive_src}",
            file=sys.stderr,
        )
        raise SystemExit(2)

    dest = manifest_root / deps_path
    dest.parent.mkdir(parents=True, exist_ok=True)
    # Copy bytes (not move) so a partial failure leaves the artifact
    # intact for inspection.
    shutil.copyfile(archive_src, dest)
    print(f"  vendored {dest} ({size} bytes, sha256 {sha256[:16]}...)")

    platform_key = TARGET_TO_PLATFORM_KEY.get(target)
    if platform_key is None:
        print(
            f"  warning: no platform-key mapping for target {target}; "
            f"deps/ file vendored but manifest.json not patched",
            file=sys.stderr,
        )
        return

    tool_manifest = manifest_root / tool / "manifest.json"
    url = lfs_url_for(deps_path)
    patched = patch_per_tool_manifest(
        tool_manifest,
        tool=tool,
        version=version,
        platform_key=platform_key,
        archive_name=archive_name,
        url=url,
        size=size,
        sha256=sha256,
    )
    if patched:
        print(f"  patched {tool_manifest} platforms[{platform_key}]")
    else:
        print(
            f"  no-op: {tool_manifest} platforms[{platform_key}] already up-to-date"
        )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--prebuilts-dir",
        type=Path,
        required=True,
        help="Directory containing the downloaded prebuilt artifacts "
             "(each as <archive>.tar.zst + <archive>.tar.zst.meta.json).",
    )
    parser.add_argument(
        "--manifest-checkout",
        type=Path,
        required=True,
        help="Path to a local checkout of the soldr `manifest` branch.",
    )
    args = parser.parse_args(argv)

    prebuilts_dir: Path = args.prebuilts_dir.resolve()
    manifest_root: Path = args.manifest_checkout.resolve()
    if not prebuilts_dir.is_dir():
        print(f"error: --prebuilts-dir {prebuilts_dir} is not a directory", file=sys.stderr)
        return 2
    if not manifest_root.is_dir():
        print(f"error: --manifest-checkout {manifest_root} is not a directory", file=sys.stderr)
        return 2

    metas = sorted(prebuilts_dir.glob("*.meta.json"))
    if not metas:
        print(f"no *.meta.json files under {prebuilts_dir}; nothing to install")
        return 0

    print(f"installing {len(metas)} prebuilt(s) into {manifest_root}")
    for meta_path in metas:
        install_one(meta_path, manifest_root)
    print("done")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
