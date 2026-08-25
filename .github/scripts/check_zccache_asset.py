#!/usr/bin/env python3
"""The locked zccache version must have a published release asset.

soldr#2164 moved the zccache pin to a version with no published
release. Every local signal stayed green — builds, ~1460 tests, clippy,
`loc_ratchet` — because **none of them exercise that
fetch**. `bootstrap + linux-x86` went red on `main` instead.

That is not an oversight in any of those checks. The crate is compiled
*into* soldr, so a source-level bump needs nothing from the network; but release
staging separately **downloads a prebuilt zccache keyed on the locked crate's
version**:

```bash
zccache_version=$(python3 .github/scripts/zccache_version.py)
```

So the pin cannot lead the release, and nothing enforced that. CLAUDE.md
documents the pre-flight query a human is supposed to run before bumping. This
is that query, run by CI instead of by memory.

## What is checked

1. That the resolved zccache version can be read from `Cargo.lock` at all.

   This used to also compare `_vender/zccache/Cargo.toml`'s version against the
   lock's, because the release read the manifest while the build resolved the
   lock and the two could drift. soldr#2837 removed that manifest — zccache is
   an exact crates.io dependency now, so the lock is the only source and there
   is nothing left to disagree with it.
2. That version has a published asset for **every** platform release staging
   asks for. `cross-compile-all-targets.yml` maps six target triples onto asset
   queries; a version missing any one of them breaks that lane, so checking only
   the linux/musl example from CLAUDE.md would still let five of the six through.

## Network policy

A manifest that cannot be fetched is **not** a failure. This guard runs on every
PR, and failing them all on a GitHub Pages blip would train people to ignore it.
A fetched manifest that lacks the version *is* a failure — that is the defect,
and it is not transient.
"""

from __future__ import annotations

import argparse
import pathlib
import re
import sys

# Reuse the selection logic rather than restating it: a second implementation of
# "does this manifest offer that asset" could disagree with the one release
# staging actually uses, which would make this guard worse than nothing.
# Resolvable because Python puts a script's own directory on sys.path, which
# is how the sibling scripts here import it too.
from toolchain_asset_query import (
    DEFAULT_ORIGIN,
    find_asset,
    find_release,
    load_tool_manifest,
    normalize_arch,
    normalize_os,
    platform_candidates,
)

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]

# Mirrors the target -> query mapping in `cross-compile-all-targets.yml`. zccache
# ships linux as musl-only (static, glibc-compatible), hence no gnu row.
REQUIRED_PLATFORMS: tuple[tuple[str, str, str | None], ...] = (
    ("linux", "x86", "musl"),
    ("linux", "arm", "musl"),
    ("mac", "x86", None),
    ("mac", "arm", None),
    ("windows", "x86", "msvc"),
    ("windows", "arm", "msvc"),
)


def locked_version(repo_root: pathlib.Path, package: str = "zccache") -> str | None:
    """The version the build resolves, from `Cargo.lock`."""
    try:
        contents = (repo_root / "Cargo.lock").read_text(encoding="utf-8")
    except OSError:
        return None
    pattern = re.compile(
        r'^name = "' + re.escape(package) + r'"\n^version = "([^"]+)"',
        re.MULTILINE,
    )
    match = pattern.search(contents)
    return match.group(1) if match else None


def missing_platforms(payload: dict, version: str) -> list[tuple[str, str, str | None]]:
    """Which required platforms this manifest does not offer for `version`.

    Raises `SystemExit` (from `find_release`) when the version itself is absent;
    the caller distinguishes that from a partial platform set because the two
    have different remedies — publish a release, versus publish more assets for
    an existing one.
    """
    release = find_release(payload, version)
    missing = []
    for platform, arch, extra in REQUIRED_PLATFORMS:
        candidates = platform_candidates(
            normalize_os(platform), normalize_arch(arch), extra
        )
        try:
            find_asset(release, candidates, require_sha256=True)
        except SystemExit:
            missing.append((platform, arch, extra))
    return missing


def describe(platform: str, arch: str, extra: str | None) -> str:
    return f"{platform}/{arch}" + (f"/{extra}" if extra else "")


def published_versions(payload: dict, limit: int = 8) -> list[str]:
    releases = payload.get("releases")
    if not isinstance(releases, list):
        return []
    return [
        str(release.get("version"))
        for release in releases[:limit]
        if isinstance(release, dict) and release.get("version")
    ]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo-root", type=pathlib.Path, default=REPO_ROOT)
    parser.add_argument("--origin", default=DEFAULT_ORIGIN)
    args = parser.parse_args()

    version = locked_version(args.repo_root)
    if version is None:
        print(
            "error: could not read the resolved zccache version from Cargo.lock.\n"
            "Refresh the lock through Soldr and commit it."
        )
        return 1

    try:
        url, payload = load_tool_manifest(args.origin, "zccache")
    except (OSError, ValueError, SystemExit) as exc:
        # Not a failure: see the network policy in this module's docstring.
        print(f"check_zccache_asset: skipped, cannot resolve zccache manifest ({exc})")
        return 0

    if not isinstance(payload, dict):
        print(f"check_zccache_asset: skipped, {url} is not a JSON object")
        return 0

    try:
        missing = missing_platforms(payload, version)
    except SystemExit:
        available = published_versions(payload)
        print(
            f"error: locked zccache is {version}, which has no published "
            f"release in the tool manifest.\n\n"
            "The pin cannot lead the release: `cross-compile-all-targets.yml`\n"
            "downloads a prebuilt zccache keyed on this exact version, so a\n"
            "source-only bump passes every local check and fails on main\n"
            "(soldr#2164).\n\n"
            f"Most recent published versions: {', '.join(available) or '(none)'}\n"
            "Publish a zccache release for this version first, or move the pin\n"
            "to one that exists."
        )
        return 1

    if missing:
        names = ", ".join(describe(*entry) for entry in missing)
        print(
            f"error: zccache {version} is published but is missing assets for: "
            f"{names}.\n\n"
            "Release staging queries one asset per target triple, so a partial\n"
            "platform set fails that lane for the targets it omits rather than\n"
            "failing outright here.\n"
            "Publish the missing assets, or move the pin to a complete release."
        )
        return 1

    print(
        f"check_zccache_asset: zccache {version} has assets for all "
        f"{len(REQUIRED_PLATFORMS)} required platforms."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
