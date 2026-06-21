# soldr `manifest` branch

This is a long-lived **orphan branch** — it shares no history with `main`. Its
tree is a public catalogue of the third-party tool releases that soldr's CI
consumes.

```
/                              # root
├── manifest.json              # top-level index: tools -> subdir
├── zccache/manifest.json      # one per tool: full release history + assets
├── crgx/manifest.json
├── cargo-chef/manifest.json
├── cargo-zigbuild/manifest.json
└── cargo-xwin/manifest.json
```

## Why this branch exists

The `cross-compile-all-targets.yml` workflow used to have every parallel matrix
lane independently resolve tool download URLs against the GitHub Releases REST
API. With 7+ lanes hitting the same runner IP, the unauthenticated 60-req/hour
quota burned out fast and the workflow 403-ed.

The manifest branch breaks that:

1. A nightly workflow on `main` (`.github/workflows/refresh-manifest.yml`)
   runs `.github/scripts/build_manifest.py`, which queries the GitHub API
   **once per tool** (authenticated, 5000 req/hour, `per_page=100`) and writes
   the resolved `browser_download_url` for every asset into the per-tool files
   here. New releases get **merged into** the existing file — older tags are
   preserved as a permanent archive.
2. The workflow commits any diff back to this branch.
3. Consumer workflows fetch from
   `https://raw.githubusercontent.com/zackees/soldr/manifest/<path>` — that
   URL is CDN-served and **not** subject to the API rate limit.

Per-tool files only change when upstream actually publishes something new (the
script uses content equality, not timestamps). `git log` here is the source of
truth for "when did this tool's release set last change."

## Getting the latest release for a tool (dead simple)

```python
import json, urllib.request
manifest = json.loads(urllib.request.urlopen(
    "https://raw.githubusercontent.com/zackees/soldr/manifest/zccache/manifest.json"
).read())

latest_tag = manifest["latest"]               # always set
release    = manifest["releases"][latest_tag]  # full release entry
url        = release["assets"]["zccache-v1.12.9-x86_64-unknown-linux-musl.tar.gz"]["url"]
```

No sort, no semver parsing, no special-case logic — `latest` always points at
the newest release on file.

## Schema

Top-level (`/manifest.json`) — index only, no asset URLs:

```json
{
  "schema_version": 2,
  "tools": {
    "<tool-name>": {
      "path":         "<tool-name>/manifest.json",
      "owner":        "zackees",
      "repo":         "zccache",
      "latest":       "1.12.9",
      "pinned":       "1.12.9",
      "tracked_tags": ["1.12.9", "1.12.8", "..."]
    }
  }
}
```

Per-tool (`<tool-name>/manifest.json`):

```json
{
  "schema_version": 2,
  "name":           "zccache",
  "owner":          "zackees",
  "repo":           "zccache",
  "latest":         "1.12.9",        // == tracked_tags[0]
  "pinned":         "1.12.9",        // soldr's MANAGED_<TOOL>_VERSION (or null)
  "tracked_tags":   ["1.12.9", "1.12.8", "..."],
  "releases": {
    "1.12.9": {
      "tag":              "1.12.9",
      "version":          "1.12.9",
      "name":             "v1.12.9",
      "draft":            false,
      "prerelease":       false,
      "created_at":       "...",
      "published_at":     "...",
      "release_html_url": "https://github.com/.../releases/tag/1.12.9",
      "assets": {
        "<asset-filename>": {
          "url":          "https://github.com/.../releases/download/<tag>/<filename>",
          "size":         12345678,
          "content_type": "application/x-gtar",
          "created_at":   "...",
          "updated_at":   "..."
        }
      }
    }
  }
}
```

`releases` is ordered by `published_at` **descending** (newest first), so the
first entry's tag matches `latest`.

## Ordering

GitHub's `/releases?per_page=100` already returns releases sorted by
`published_at` descending. The script trusts that ordering — there is **no
client-side semver parsing**. When merging fresh API results with the existing
file (preserving releases that fell off the API's per_page window), the merged
dict is rebuilt by sorting on `published_at`, which is the same field GitHub
uses, so the result mirrors GitHub's authoritative ordering.

## Manual refresh

```sh
git checkout manifest
# from a checkout of `main` with the script in it:
GITHUB_TOKEN=$(gh auth token) \
  python3 /path/to/main/.github/scripts/build_manifest.py \
    --output-dir . \
    --repo-root /path/to/main
git add -A
git commit -m "manifest: refresh"
git push origin manifest
```

Or just trigger the `refresh-manifest.yml` workflow with `workflow_dispatch`.

## Hands off the orphan history

Don't rebase, force-push, or merge `main` into this branch. The whole point is
that the tree is content-addressable: a viewer can hash a file here and trust
it for as long as the commit stays on the branch. Rewriting history breaks
that trust.
