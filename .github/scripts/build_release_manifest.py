#!/usr/bin/env python3
"""Pre-fetch all third-party tool release URLs into a single manifest.

Why this exists: every parallel cross-compile lane in
`cross-compile-all-targets.yml` used to independently resolve tool
download URLs against the GitHub REST API (60 req/hour unauthenticated).
With 7 lanes × 5 tools that adds up to a burst of >35 API calls, which
on a busy runner IP routinely hits the 403 rate-limit cap. Centralizing
the resolution into a single Stage 1 step (which DOES use
`$GITHUB_TOKEN` — authenticated rate limit is 1000 req/hour, plenty of
headroom) means Stage 2 lanes never touch the API; they curl public
`https://github.com/.../releases/download/...` URLs straight from the
manifest. Those public URLs are CDN-backed and aren't subject to the
API rate limit.

The manifest is uploaded as an artifact AND saved to actions/cache with
a day-keyed key so re-runs within the same UTC day skip the API round
trip entirely.

Schema (`release-manifest.json`):

    {
      "schema_version": 1,
      "generated_at": "2026-06-21T05:30:00Z",
      "tools": {
        "<tool-name>": {
          "version": "1.12.9",
          "tag": "1.12.9",
          "release_html_url": "https://github.com/.../releases/tag/1.12.9",
          "assets": {
            "<asset-filename>": {
              "url":  "https://github.com/.../releases/download/<tag>/<filename>",
              "size": 12345678
            }
          }
        }
      }
    }

`url` is always the public `browser_download_url` (NOT the API
`api.github.com/.../releases/assets/<id>` URL — that one IS rate-limited
and counts against the consumer's quota).
"""

from __future__ import annotations

import json
import os
import re
import sys
import time
import urllib.error
import urllib.request
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

REPO_ROOT = Path(__file__).resolve().parents[2]


def read_constant(path: Path, name: str) -> str:
    """Pull a `const NAME: &str = "value";` out of a Rust source file."""
    text = path.read_text(encoding="utf-8")
    m = re.search(rf'{re.escape(name)}\s*:\s*&str\s*=\s*"([^"]+)"', text)
    if not m:
        raise RuntimeError(f"could not find {name} in {path}")
    return m.group(1)


def gh_request(url: str, token: str | None) -> Any:
    req = urllib.request.Request(url)
    req.add_header("Accept", "application/vnd.github+json")
    req.add_header("X-GitHub-Api-Version", "2022-11-28")
    req.add_header("User-Agent", "soldr-release-manifest-builder")
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    last_exc: Exception | None = None
    for attempt in range(5):
        try:
            with urllib.request.urlopen(req, timeout=30) as resp:
                return json.loads(resp.read().decode("utf-8"))
        except urllib.error.HTTPError as exc:
            last_exc = exc
            # 403 + secondary-rate-limit headers → back off and retry.
            if exc.code in (403, 429):
                wait = 2 ** attempt
                print(
                    f"  github API returned {exc.code}; sleeping {wait}s before retry",
                    file=sys.stderr,
                )
                time.sleep(wait)
                continue
            raise
        except urllib.error.URLError as exc:
            last_exc = exc
            wait = 2 ** attempt
            time.sleep(wait)
    raise RuntimeError(f"github API failed after retries: {last_exc}")


def resolve_release(owner: str, repo: str, tag: str | None, token: str | None) -> dict[str, Any]:
    """Fetch a release by tag, or `latest` when tag is None.

    `tag` is the literal git tag (e.g. `1.12.9` for zccache, `v0.23.0`
    for cargo-zigbuild). Callers pass the exact pinned tag — there's no
    semver resolution here.
    """
    if tag is None:
        url = f"https://api.github.com/repos/{owner}/{repo}/releases/latest"
    else:
        url = f"https://api.github.com/repos/{owner}/{repo}/releases/tags/{tag}"
    return gh_request(url, token)


def build_tool_entry(
    name: str,
    owner: str,
    repo: str,
    tag: str | None,
    token: str | None,
) -> dict[str, Any]:
    print(f"resolving {name} @ {tag or 'latest'} ({owner}/{repo})...", file=sys.stderr)
    release = resolve_release(owner, repo, tag, token)
    resolved_tag: str = release["tag_name"]
    # The version commonly drops the leading `v` for human display, but
    # we keep both shapes available for consumers that prefer one or
    # the other.
    version = resolved_tag[1:] if resolved_tag.startswith("v") else resolved_tag

    assets: dict[str, dict[str, Any]] = {}
    for asset in release.get("assets", []):
        # `browser_download_url` is the public CDN-backed URL that does
        # NOT count against the consumer's API rate limit. That's the
        # entire point of generating this manifest centrally.
        assets[asset["name"]] = {
            "url": asset["browser_download_url"],
            "size": asset.get("size"),
        }
    return {
        "version": version,
        "tag": resolved_tag,
        "release_html_url": release.get("html_url"),
        "assets": assets,
    }


def main() -> int:
    out_path = Path(sys.argv[1]) if len(sys.argv) > 1 else Path("release-manifest.json")
    token = os.environ.get("GITHUB_TOKEN") or None

    # Resolve pinned versions from soldr's source so the manifest stays
    # in lockstep with what the cross builders actually consume.
    fetch_mod = REPO_ROOT / "crates" / "soldr-cli" / "src" / "fetch" / "mod.rs"
    known_tools = REPO_ROOT / "crates" / "soldr-cli" / "src" / "fetch" / "known_tools.rs"

    zccache_version = read_constant(fetch_mod, "MANAGED_ZCCACHE_VERSION")
    crgx_version = read_constant(fetch_mod, "MANAGED_CRGX_VERSION")
    cargo_chef_version = read_constant(known_tools, "CARGO_CHEF_PINNED_VERSION")

    # Each (owner, repo, tag) below maps to one GitHub release lookup.
    # cargo-zigbuild / cargo-xwin are unpinned in known_tools (latest at
    # `soldr cargo zigbuild` invocation time), so we resolve `latest`
    # here and the consumer-side soldr binary picks the same version up
    # off PATH before it would have called the API itself.
    tools = [
        ("zccache",        "zackees",         "zccache",        zccache_version),
        ("crgx",           "yfedoseev",       "crgx",           f"v{crgx_version}"),
        ("cargo-chef",     "LukeMathWalker",  "cargo-chef",     f"v{cargo_chef_version}"),
        ("cargo-zigbuild", "rust-cross",      "cargo-zigbuild", None),
        ("cargo-xwin",     "rust-cross",      "cargo-xwin",     None),
    ]

    out = {
        "schema_version": 1,
        "generated_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "tools": {},
    }

    for name, owner, repo, tag in tools:
        out["tools"][name] = build_tool_entry(name, owner, repo, tag, token)

    out_path.write_text(json.dumps(out, indent=2) + "\n", encoding="utf-8")
    total_assets = sum(len(t["assets"]) for t in out["tools"].values())
    print(
        f"wrote {out_path} ({len(out['tools'])} tools, {total_assets} assets)",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
