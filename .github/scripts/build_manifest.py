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
# Schema 2 (vs the bootstrap commit's schema 1): per-tool files carry a
# `releases` map keyed by tag instead of a single flat release. The
# nightly refresh MERGES the newly-fetched release into that map rather
# than replacing the file — older tags stay around so a CI workflow
# that pinned an older soldr can still resolve its assets from the
# manifest. See README.md on the `manifest` branch for the rationale.
SCHEMA_VERSION = 2


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


def build_release_entry(release: dict[str, Any]) -> dict[str, Any]:
    """Render one release into the `releases[<tag>]` shape stored in
    the per-tool manifest. Captures every date field GitHub provides
    so consumers don't need a second API call to learn "when did this
    release ship?" or "when was this asset re-uploaded?"
    Asset names are sorted alphabetically inside each release so the
    diff stays deterministic between refreshes.
    """
    resolved_tag: str = release["tag_name"]
    version = resolved_tag[1:] if resolved_tag.startswith("v") else resolved_tag
    assets: dict[str, dict[str, Any]] = {}
    for asset in release.get("assets", []):
        # `browser_download_url` is the public CDN-backed URL — does
        # NOT count against the consumer's API rate limit. That's the
        # whole point of centralising the lookup here.
        assets[asset["name"]] = {
            "url": asset["browser_download_url"],
            "size": asset.get("size"),
            "content_type": asset.get("content_type"),
            "created_at": asset.get("created_at"),
            "updated_at": asset.get("updated_at"),
        }
    return {
        "tag": resolved_tag,
        "version": version,
        "name": release.get("name"),
        "draft": release.get("draft"),
        "prerelease": release.get("prerelease"),
        "created_at": release.get("created_at"),
        "published_at": release.get("published_at"),
        "release_html_url": release.get("html_url"),
        "assets": dict(sorted(assets.items())),
    }


def load_existing_per_tool(path: Path) -> dict[str, Any] | None:
    """Read a previously-written per-tool manifest, if present.

    Returns the parsed JSON dict on success, None if the file is
    missing or unreadable. Failures are tolerated (the merge logic
    treats them as "start fresh"); the next write will normalise the
    file.
    """
    if not path.is_file():
        return None
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None


def build_merged_tool_manifest(
    name: str,
    owner: str,
    repo: str,
    pinned_tag: str | None,
    token: str | None,
    existing: dict[str, Any] | None,
) -> dict[str, Any]:
    """Fetch up to 100 releases and merge them into the existing
    per-tool manifest. Entries the API just returned overwrite their
    counterparts on file; entries that aren't in the API window are
    preserved from the prior file (this is the "merged not chopped"
    property — older historical releases stick around).

    Ordering: every release we store carries `published_at`. The
    merged dict is rebuilt in `published_at` DESCENDING order, so the
    first key is always the newest. That makes `manifest["latest"]`
    a trivial dictionary lookup with no client-side sort algorithm —
    GitHub already did the ordering work; we just preserve it.
    """
    print(f"listing releases for {name} ({owner}/{repo})...", file=sys.stderr)
    fetched = list_releases(owner, repo, token)

    # Start from existing releases (preserves older entries that have
    # fallen off the per_page=100 API window), then overwrite each
    # with what we just fetched.
    merged: dict[str, dict[str, Any]] = {}
    if existing is not None:
        for tag, entry in (existing.get("releases") or {}).items():
            merged[tag] = entry
    for release in fetched:
        entry = build_release_entry(release)
        merged[entry["tag"]] = entry

    # Sort by `published_at` descending so the on-disk JSON reads
    # newest-first and `tracked_tags[0]` is the newest release. Entries
    # missing `published_at` (defensive — shouldn't happen with the
    # GitHub API) sink to the bottom.
    def _key(item: tuple[str, dict[str, Any]]) -> tuple[int, str, str]:
        tag, entry = item
        published = entry.get("published_at") or ""
        # Tier 1 = has published_at; tier 0 = missing (sinks below).
        return (1 if published else 0, published, tag)

    ordered = dict(sorted(merged.items(), key=_key, reverse=True))
    tracked = list(ordered.keys())

    return {
        "schema_version": SCHEMA_VERSION,
        "name": name,
        "owner": owner,
        "repo": repo,
        # `latest` always names the newest release we know about, so:
        #   url = manifest['releases'][manifest['latest']]['assets'][<asset-name>]['url']
        # If this tool has a soldr-side pin, `pinned` records it so
        # consumers can resolve the pinned tag's assets just as cheaply:
        #   url = manifest['releases'][manifest['pinned']]['assets'][...]
        "latest": tracked[0] if tracked else None,
        "pinned": pinned_tag,
        "tracked_tags": tracked,
        "releases": ordered,
    }


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
        manifest = build_merged_tool_manifest(
            name, owner, repo, pinned_tag, token, existing
        )
        per_tool_index[name] = {
            "path": f"{name}/{PER_TOOL_FILENAME}",
            "owner": manifest["owner"],
            "repo": manifest["repo"],
            "latest": manifest["latest"],
            "pinned": manifest["pinned"],
            "tracked_tags": manifest["tracked_tags"],
        }
        per_tool_payload = json.dumps(manifest, indent=2, sort_keys=False) + "\n"
        if write_if_changed(tool_path, per_tool_payload):
            print(
                f"  wrote {tool_path} (latest={manifest['latest']}, "
                f"{len(manifest['tracked_tags'])} tags total)",
                file=sys.stderr,
            )
            changed_count += 1
        else:
            print(f"  unchanged {tool_path}", file=sys.stderr)

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
