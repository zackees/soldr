#!/usr/bin/env python3
"""Generate the hierarchical release manifest for the `manifest` branch.

The `manifest` branch in this repo is a long-lived orphan branch whose
tree mirrors a public release-asset catalogue:

    /                              # root of the `manifest` branch
    ├── manifest.json              # top-level index: tools -> subdir
    ├── zccache/manifest.json      # one per tool: assets + URLs + sha
    ├── crgx/manifest.json
    ├── cargo-chef/manifest.json
    ├── cargo-zigbuild/manifest.json
    └── cargo-xwin/manifest.json

A nightly workflow (`.github/workflows/refresh-manifest.yml`) re-runs
this script and commits the diff — so per-tool files only change when
the upstream release actually changes. Workflows on `main` consume the
manifest via `https://raw.githubusercontent.com/<owner>/<repo>/manifest/...`,
which is CDN-served and not subject to the GitHub Releases API
rate-limit that was triggering 403s across parallel matrix jobs.

Run this script either:
  - inside a checkout of the `manifest` branch with no `--output-dir`,
    so it rewrites the branch in place, OR
  - with `--output-dir <path>` to dump the tree somewhere else (used
    by the initial bootstrap step when the `manifest` branch does not
    yet exist).

Auth: `$GITHUB_TOKEN` (workflow-provided) raises the API rate limit
from the unauthenticated 60 req/hour to 5000 req/hour. The script
back-offs on 403/429 with exponential delay.

Per-tool pinned versions are read directly out of soldr's own source
constants so the manifest can never drift from what soldr would fetch.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any

# Ordering note: GitHub's `/releases?per_page=N` endpoint already
# returns releases sorted by `published_at` descending (newest-first),
# so we don't parse versions client-side — we just trust the API order
# and break ties on `published_at` when merging old + new entries. No
# `packaging` dep, no hand-rolled semver tuple, no edge cases around
# backport releases.

REPO_ROOT = Path(__file__).resolve().parents[2]
TOP_LEVEL_FILENAME = "manifest.json"
PER_TOOL_FILENAME = "manifest.json"
# Schema 5 (vs v4): per-tool manifests are a FLAT JSON ARRAY of
# self-describing release dicts. No outer metadata wrapper. First
# element is always the latest release (array sorted by
# `published_at` descending). Each dict carries the tool name, owner,
# repo, tag, version, dates, plus a normalized `platforms` map and
# the raw upstream `assets` map.
#
# Consumer recipes:
#
#     # latest URL for a host
#     jq -r '.[0].platforms["linux-x64-musl"].url' zccache/manifest.json
#
#     # specific tag's URL
#     jq -r '.[] | select(.tag == "1.12.9") | .platforms["linux-x64-musl"].url' zccache/manifest.json
#
#     # grep-friendly — every entry is self-describing
#     grep -l '"tool": "zccache"' */manifest.json
#
# The top-level `/manifest.json` index is still a DICT (it's an index,
# not a release list); it carries `schema_version` and per-tool
# pointers + summary metadata.
SCHEMA_VERSION = 5


def read_constant(path: Path, name: str) -> str:
    text = path.read_text(encoding="utf-8")
    m = re.search(rf'{re.escape(name)}\s*:\s*&str\s*=\s*"([^"]+)"', text)
    if not m:
        raise RuntimeError(f"could not find {name} in {path}")
    return m.group(1)


def gh_request(url: str, token: str | None) -> Any:
    req = urllib.request.Request(url)
    req.add_header("Accept", "application/vnd.github+json")
    req.add_header("X-GitHub-Api-Version", "2022-11-28")
    req.add_header("User-Agent", "soldr-manifest-builder")
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    last_exc: Exception | None = None
    for attempt in range(6):
        try:
            with urllib.request.urlopen(req, timeout=30) as resp:
                return json.loads(resp.read().decode("utf-8"))
        except urllib.error.HTTPError as exc:
            last_exc = exc
            if exc.code in (403, 429):
                wait = int(exc.headers.get("Retry-After") or 2 ** attempt)
                print(
                    f"  github API {exc.code} for {url}; sleeping {wait}s",
                    file=sys.stderr,
                )
                time.sleep(wait)
                continue
            raise
        except urllib.error.URLError as exc:
            last_exc = exc
            time.sleep(2 ** attempt)
    raise RuntimeError(f"github API failed after retries: {last_exc}")


def list_releases(owner: str, repo: str, token: str | None) -> list[dict[str, Any]]:
    """Fetch the most recent 100 releases for a repo.

    GitHub returns them sorted by `published_at` descending (newest
    first). 100 is the max `per_page` for this endpoint and covers
    every tool we currently track (highest is cargo-chef at ~75
    releases). If a tool ever exceeds 100 historical releases, the
    older entries already on the manifest branch are preserved by the
    merge logic below — only entries that fall off the API window stop
    receiving updates.
    """
    url = f"https://api.github.com/repos/{owner}/{repo}/releases?per_page=100"
    return gh_request(url, token)


def derive_platform_key(filename: str) -> str | None:
    """Map an upstream asset filename to a normalized `<os>-<arch>[-<extra>]`
    key, or None if the filename clearly isn't a runnable platform binary
    (sha256 sums, installers, source tarballs, dist manifests, etc.).

    The key shape uses modern short arch names (npm/Node.js convention):
      os    ∈ { linux, darwin, windows }
      arch  ∈ { x64, arm64, armv7, i686, universal2 }
      extra ∈ { gnu, musl, musleabi, musleabihf, msvc, gnullvm }  (optional)

    `extra` is only included when it's a meaningful disambiguator —
    e.g. `linux-x64-gnu` vs `linux-x64-musl`, or `windows-arm64-msvc`
    vs `windows-arm64-gnullvm`. `darwin-*` and Windows tools that ship
    only one ABI default the extra appropriately.

    Examples (rust-target-triple style and abbreviated style both work):
      x86_64-unknown-linux-gnu        → linux-x64-gnu
      x86_64-unknown-linux-musl       → linux-x64-musl
      aarch64-pc-windows-msvc         → windows-arm64-msvc
      x86_64-apple-darwin             → darwin-x64
      aarch64-apple-darwin            → darwin-arm64
      windows-x64                     → windows-x64-msvc   (xwin's shape)
      windows-x86                     → windows-i686-msvc  (xwin's shape)
      universal2-apple-darwin         → darwin-universal2
    """
    name = filename.lower()

    # Drop obvious non-binary artifacts up front. Anything not a tarball
    # or zip-style archive is skipped.
    if not (
        name.endswith(".tar.gz")
        or name.endswith(".tgz")
        or name.endswith(".tar.xz")
        or name.endswith(".txz")
        or name.endswith(".tar.bz2")
        or name.endswith(".zip")
    ):
        return None
    if "source" in name or "dist-manifest" in name or "installer" in name:
        return None
    # Debug / symbol packages aren't the canonical platform binary for
    # consumer use — zccache ships an `*-debug.tar.gz` next to every
    # platform asset, which would otherwise win the platform-key
    # contest alphabetically. The release's raw `assets` dict still
    # carries these by their full filename.
    if "-debug" in name or ".debug" in name or "-sym" in name or ".pdb" in name:
        return None

    # OS detection. `apple-darwin` and `macos` both map to darwin.
    if "apple-darwin" in name or "-macos-" in name or ".macos." in name:
        os_key = "darwin"
    elif "windows" in name:
        os_key = "windows"
    elif "linux" in name:
        os_key = "linux"
    else:
        return None

    # Arch detection — modern short names only. 32-bit lanes are
    # intentionally dropped: every modern process is 64-bit, the
    # manifest shouldn't fragment its schema to surface i686 / armv7
    # binaries that nobody runs in production anymore.
    if "universal2" in name:
        # Apple's fat binary — covers both darwin-x64 and darwin-arm64.
        # Kept because cargo-xwin and some other tools ship this format
        # and consumers may not have an architecture-specific build to
        # fall back on.
        arch = "universal2"
    elif "x86_64" in name or "windows-x64" in name or "amd64" in name:
        arch = "x64"
    elif "aarch64" in name or "arm64" in name:
        arch = "arm64"
    else:
        # i686 / armv7 / armhf / 32-bit anything: not surfaced.
        return None

    # ABI / extra. We surface this whenever it's a meaningful
    # disambiguator. For windows tools that ship only an MSVC variant
    # (e.g. cargo-xwin's `windows-x64.zip`), assume MSVC — that's the
    # mainstream Windows ABI per soldr's "MSVC on Windows always" rule.
    extra: str | None = None
    if "musleabihf" in name:
        extra = "musleabihf"
    elif "musleabi" in name:
        extra = "musleabi"
    elif "musl" in name:
        extra = "musl"
    elif "gnullvm" in name:
        extra = "gnullvm"
    elif "-gnu" in name or ".gnu." in name:
        extra = "gnu"
    elif "msvc" in name:
        extra = "msvc"
    elif os_key == "windows":
        # Windows tools that ship a single un-suffixed Windows variant
        # (cargo-xwin's `windows-x64.zip` / `windows-x86.zip`) are MSVC
        # — Microsoft's mainstream Windows ABI.
        extra = "msvc"

    if extra is not None:
        return f"{os_key}-{arch}-{extra}"
    return f"{os_key}-{arch}"


def build_release_entry(
    release: dict[str, Any],
    *,
    tool: str | None = None,
    owner: str | None = None,
    repo: str | None = None,
) -> dict[str, Any]:
    """Render one release into the `releases[<tag>]` shape stored in
    the per-tool manifest. Captures every date field GitHub provides
    so consumers don't need a second API call to learn "when did this
    release ship?" or "when was this asset re-uploaded?"
    Asset names are sorted alphabetically inside each release so the
    diff stays deterministic between refreshes.

    `platforms` is a sibling dict keyed by normalized
    `<os>-<arch>[-<extra>]` strings. Consumers query that instead of
    parsing per-tool filename quirks: every release's host-x86_64
    Linux binary lives under `platforms["linux-x86_64-gnu"]` (or
    `linux-x86_64-musl` for tools that only ship musl).
    """
    resolved_tag: str = release["tag_name"]
    version = resolved_tag[1:] if resolved_tag.startswith("v") else resolved_tag
    assets: dict[str, dict[str, Any]] = {}
    platforms: dict[str, dict[str, Any]] = {}
    for asset in release.get("assets", []):
        asset_name = asset["name"]
        # `browser_download_url` is the public CDN-backed URL — does
        # NOT count against the consumer's API rate limit. That's the
        # whole point of centralising the lookup here.
        entry = {
            "url": asset["browser_download_url"],
            "size": asset.get("size"),
            "content_type": asset.get("content_type"),
            "created_at": asset.get("created_at"),
            "updated_at": asset.get("updated_at"),
        }
        assets[asset_name] = entry
        platform_key = derive_platform_key(asset_name)
        if platform_key is not None:
            # If two assets normalize to the same key (rare but
            # possible — e.g. accidentally re-uploaded variants), the
            # alphabetically-first asset name wins (sorted iteration
            # happens after this loop). That makes the choice
            # deterministic across runs.
            platforms.setdefault(
                platform_key,
                {
                    "filename": asset_name,
                    "url": entry["url"],
                    "size": entry["size"],
                },
            )
    entry: dict[str, Any] = {}
    # Self-description fields come first so a human or grep sees
    # tool/tag immediately.
    if tool is not None:
        entry["tool"] = tool
    if owner is not None:
        entry["owner"] = owner
    if repo is not None:
        entry["repo"] = repo
    entry.update({
        "tag": resolved_tag,
        "version": version,
        "name": release.get("name"),
        "draft": release.get("draft"),
        "prerelease": release.get("prerelease"),
        "created_at": release.get("created_at"),
        "published_at": release.get("published_at"),
        "release_html_url": release.get("html_url"),
        "platforms": dict(sorted(platforms.items())),
        "assets": dict(sorted(assets.items())),
    })
    return entry


def load_existing_per_tool(path: Path) -> list[dict[str, Any]]:
    """Read a previously-written per-tool manifest as a list of release
    entries. Handles every schema we've shipped:

      v5 (current): a flat JSON array — return it as-is.
      v4:           dict with version tags as top-level keys + `latest`
                    duplicate — extract the version-keyed entries.
      v3 / v2:      dict with `releases:` wrapper — extract the inner map.

    Failures (missing file, parse error) return an empty list so the
    merge logic treats them as "start fresh".
    """
    if not path.is_file():
        return []
    try:
        parsed = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return []
    if isinstance(parsed, list):
        # v5 — already a list of release dicts.
        return [e for e in parsed if isinstance(e, dict) and "tag" in e]
    if not isinstance(parsed, dict):
        return []
    # v3 / v2 — releases nested under `releases:`.
    nested = parsed.get("releases")
    if isinstance(nested, dict):
        return [e for e in nested.values() if isinstance(e, dict) and "tag" in e]
    # v4 — version tags at the top level alongside metadata. Skip the
    # `latest` duplicate (it gets regenerated from the newest entry).
    legacy_metadata = {"schema_version", "name", "owner", "repo",
                       "pinned", "tracked_tags", "latest"}
    entries: list[dict[str, Any]] = []
    for key, value in parsed.items():
        if key in legacy_metadata:
            continue
        if isinstance(value, dict) and "tag" in value and "platforms" in value:
            entries.append(value)
    return entries


def build_merged_tool_releases(
    name: str,
    owner: str,
    repo: str,
    pinned_tag: str | None,
    token: str | None,
    existing: list[dict[str, Any]],
) -> tuple[list[dict[str, Any]], str | None]:
    """Fetch up to 100 releases and merge them into the existing
    per-tool manifest. Entries the API just returned overwrite their
    counterparts on file; entries that aren't in the API window are
    preserved from the prior file (the "merged not chopped" property —
    older historical releases stick around).

    Returns `(entries, latest_tag)` where `entries` is the sorted list
    (newest-first by published_at) ready to be JSON-dumped as the
    per-tool manifest's flat array. `latest_tag` is the first entry's
    tag (or None if no releases at all).
    """
    print(f"listing releases for {name} ({owner}/{repo})...", file=sys.stderr)
    fetched = list_releases(owner, repo, token)

    # Start from prior entries (preserves older releases that fell off
    # the per_page=100 API window), keyed by tag for overwrite. Each
    # prior entry was previously written with tool/owner/repo, but we
    # re-normalize all of them so a schema bump or a tool rename
    # propagates consistently.
    by_tag: dict[str, dict[str, Any]] = {}
    for prior in existing:
        prior_tag = prior.get("tag")
        if prior_tag:
            # Re-inject tool/owner/repo in case they were missing on a
            # legacy entry.
            prior.setdefault("tool", name)
            prior.setdefault("owner", owner)
            prior.setdefault("repo", repo)
            by_tag[prior_tag] = prior
    for release in fetched:
        entry = build_release_entry(release, tool=name, owner=owner, repo=repo)
        by_tag[entry["tag"]] = entry

    def _key(entry: dict[str, Any]) -> tuple[int, str, str]:
        published = entry.get("published_at") or ""
        return (1 if published else 0, published, entry.get("tag") or "")

    ordered = sorted(by_tag.values(), key=_key, reverse=True)
    latest_tag = ordered[0]["tag"] if ordered else None
    # `pinned_tag` is captured by the caller into the top-level index;
    # we don't have to embed it in every entry, but stash it on the
    # release entry that matches the pin so a single-file consumer can
    # find the pinned release with a one-line filter:
    #     jq '.[] | select(.is_pinned)' zccache/manifest.json
    if pinned_tag is not None:
        for entry in ordered:
            entry["is_pinned"] = (entry.get("tag") == pinned_tag)
    return ordered, latest_tag


def preserve_vendored_top_level_entries(
    output_dir: Path,
    per_tool_index: dict[str, dict[str, Any]],
) -> None:
    """Re-add vendored / non-GitHub-Releases entries from the EXISTING
    root manifest.json into the in-progress index so the nightly
    refresh doesn't wipe them.

    The Apple SDK (manifest branch's `deps/mac/manifest.json`, indexed
    as `apple-sdk` at the top level) is the canonical example —
    populated by a manual procedure (extracted from
    messense/cargo-zigbuild:0.20.0) and committed once, but invisible
    to `build_merged_tool_releases` because it doesn't come from a
    GitHub release.

    Strategy: walk the OLD top-level manifest.json (if present), keep
    every entry whose `path` exists on disk and isn't already in the
    fresh index. Entries that reference a now-missing file get
    silently dropped (the per-tool file was hand-deleted, so the
    index pointer is stale).
    """
    top_path = output_dir / TOP_LEVEL_FILENAME
    if not top_path.is_file():
        return
    try:
        existing = json.loads(top_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return
    existing_tools = existing.get("tools") or {}
    for name, entry in existing_tools.items():
        if name in per_tool_index:
            continue
        path_ref = entry.get("path")
        if not path_ref:
            continue
        if not (output_dir / path_ref).is_file():
            print(
                f"  dropping stale vendored entry: {name} -> {path_ref} (file missing)",
                file=sys.stderr,
            )
            continue
        per_tool_index[name] = entry
        print(
            f"  preserving vendored entry: {name} -> {path_ref}",
            file=sys.stderr,
        )


def write_if_changed(path: Path, new_content: str) -> bool:
    """Write `new_content` to `path` only if it differs from current.

    Returns True if the file was rewritten. The nightly workflow uses
    this to keep per-tool files stable across runs — `git status` only
    shows what actually changed upstream.
    """
    if path.is_file():
        existing = path.read_text(encoding="utf-8")
        if existing == new_content:
            return False
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(new_content, encoding="utf-8")
    return True


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--output-dir",
        default=".",
        help="Directory to write the manifest tree into (default: cwd).",
    )
    parser.add_argument(
        "--repo-root",
        default=str(REPO_ROOT),
        help=(
            "Path to a soldr checkout used to read pinned version "
            "constants (default: this script's repo root)."
        ),
    )
    args = parser.parse_args()

    repo_root = Path(args.repo_root).resolve()
    fetch_mod = repo_root / "crates" / "soldr-cli" / "src" / "fetch" / "mod.rs"
    known_tools = repo_root / "crates" / "soldr-cli" / "src" / "fetch" / "known_tools.rs"

    zccache_version = read_constant(fetch_mod, "MANAGED_ZCCACHE_VERSION")
    crgx_version = read_constant(fetch_mod, "MANAGED_CRGX_VERSION")
    cargo_chef_version = read_constant(known_tools, "CARGO_CHEF_PINNED_VERSION")

    # (display_name, owner, repo, tag_or_None_for_latest).
    # cargo-zigbuild and cargo-xwin are unpinned in `known_tools`
    # (soldr resolves "latest" at fetch time), so we mirror that here.
    tools = [
        ("zccache",        "zackees",         "zccache",        zccache_version),
        ("crgx",           "yfedoseev",       "crgx",           f"v{crgx_version}"),
        ("cargo-chef",     "LukeMathWalker",  "cargo-chef",     f"v{cargo_chef_version}"),
        ("cargo-zigbuild", "rust-cross",      "cargo-zigbuild", None),
        ("cargo-xwin",     "rust-cross",      "cargo-xwin",     None),
    ]

    token = os.environ.get("GITHUB_TOKEN") or None
    output_dir = Path(args.output_dir).resolve()
    output_dir.mkdir(parents=True, exist_ok=True)

    # Build per-tool files first; the top-level index references them.
    per_tool_index: dict[str, dict[str, Any]] = {}
    changed_count = 0
    for name, owner, repo, pinned_tag in tools:
        tool_dir = output_dir / name
        tool_path = tool_dir / PER_TOOL_FILENAME
        existing = load_existing_per_tool(tool_path)
        entries, latest_tag = build_merged_tool_releases(
            name, owner, repo, pinned_tag, token, existing
        )
        per_tool_index[name] = {
            "path": f"{name}/{PER_TOOL_FILENAME}",
            "owner": owner,
            "repo": repo,
            "latest": latest_tag,
            "pinned": pinned_tag,
            "tracked_tags": [e.get("tag") for e in entries if e.get("tag")],
        }
        # Per-tool file is the flat array directly — no outer wrapper.
        per_tool_payload = json.dumps(entries, indent=2) + "\n"
        if write_if_changed(tool_path, per_tool_payload):
            print(
                f"  wrote {tool_path} (latest={latest_tag}, "
                f"{len(entries)} tags total)",
                file=sys.stderr,
            )
            changed_count += 1
        else:
            print(f"  unchanged {tool_path}", file=sys.stderr)

    # Preserve vendored / non-GitHub-Releases entries (e.g. the Apple
    # SDK at deps/mac/manifest.json) that this script doesn't know how
    # to regenerate but which already exist on the manifest branch.
    # Without this, the nightly refresh wipes them from the top-level
    # index every run.
    preserve_vendored_top_level_entries(output_dir, per_tool_index)

    top_manifest = {
        "schema_version": SCHEMA_VERSION,
        "tools": dict(sorted(per_tool_index.items())),
    }
    top_path = output_dir / TOP_LEVEL_FILENAME
    # No `generated_at` field anywhere — every output file is content
    # derived from upstream releases. The nightly refresh re-runs the
    # script and `git commit` is a no-op when nothing changed, which is
    # exactly the user-asked-for property: "manifest only updates when
    # there's a change." `git log manifest -- manifest.json` is the
    # source of truth for when each refresh actually saw a change.
    top_payload = json.dumps(top_manifest, indent=2) + "\n"
    top_changed = write_if_changed(top_path, top_payload)
    if top_changed:
        print(f"  wrote {top_path}", file=sys.stderr)
    print(
        f"manifest built: {len(tools)} tools, {changed_count} per-tool files updated, "
        f"top-level {'updated' if top_changed else 'unchanged'}",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
