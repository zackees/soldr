#!/usr/bin/env python3
"""Download one soldr-toolchain asset and verify its catalogue digest.

The catalogue query intentionally exposes metadata (including SHA-256) rather
than a URL-only fast path.  Every CI consumer that extracts or executes a
catalogued archive should use this helper so a changed or substituted asset is
rejected before it reaches PATH or an archive.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import shutil
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any

from toolchain_asset_query import resolve_metadata, write_multipart_asset


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def download_verified(metadata: dict, output: Path) -> dict:
    expected = str(metadata.get("sha256", "")).lower()
    if len(expected) != 64 or any(char not in "0123456789abcdef" for char in expected):
        raise SystemExit("catalogued asset has no valid sha256")
    raw_urls = metadata.get("urls")
    raw_parts = metadata.get("parts")
    # Concrete lists, not truthiness flags: mypy does not narrow `Any | None`
    # through a separate boolean.
    urls: list[Any] = raw_urls if isinstance(raw_urls, list) else []
    parts: list[dict[str, Any]] = raw_parts if isinstance(raw_parts, list) else []
    has_urls = bool(urls)
    has_parts = bool(parts)
    if not has_urls and not has_parts:
        raise SystemExit("catalogued asset has neither a download URL nor parts")

    # Multipart is the common shape under catalogue v2 (soldr#2850); the
    # reassembly lives in `toolchain_asset_query` so this script, the nextest
    # fetcher and the tool installer share one implementation.
    if not has_urls:
        output.parent.mkdir(parents=True, exist_ok=True)
        temporary = output.with_suffix(output.suffix + ".part")
        try:
            write_multipart_asset(parts, temporary)
            actual = sha256(temporary)
            if actual != expected:
                raise SystemExit(
                    f"catalogued asset sha256 mismatch: expected {expected}, "
                    f"got {actual}"
                )
            temporary.replace(output)
        finally:
            temporary.unlink(missing_ok=True)
        return metadata

    output.parent.mkdir(parents=True, exist_ok=True)
    temporary = output.with_suffix(output.suffix + ".part")
    last_error: Exception | None = None
    try:
        for url in urls:
            try:
                request = urllib.request.Request(
                    str(url), headers={"Accept-Encoding": "identity"}
                )
                with (
                    urllib.request.urlopen(request, timeout=120) as response,
                    temporary.open("wb") as handle,
                ):
                    shutil.copyfileobj(response, handle)
                actual = sha256(temporary)
                if actual != expected:
                    raise SystemExit(
                        f"catalogued asset sha256 mismatch: expected {expected}, got {actual}"
                    )
                temporary.replace(output)
                return {**metadata, "path": str(output), "verified_sha256": actual}
            except urllib.error.URLError as exc:
                last_error = exc
                continue
    finally:
        temporary.unlink(missing_ok=True)
    raise SystemExit(f"all catalogue URLs failed: {last_error}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tool")
    parser.add_argument("--origin", default="https://zackees.github.io/soldr-toolchain")
    parser.add_argument("--platform", required=True)
    parser.add_argument("--arch", required=True)
    parser.add_argument("--extra", default=None)
    parser.add_argument("--version", default="latest")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    metadata = resolve_metadata(
        tool=args.tool,
        origin=args.origin,
        tool_manifest_url_override=None,
        platform=args.platform,
        arch=args.arch,
        extra=args.extra,
        version=args.version,
    )
    print(json.dumps(download_verified(metadata, args.output), sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
