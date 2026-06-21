# soldr `manifest` branch

This is a long-lived **orphan branch** — it shares no history with `main`. Its
tree is a public catalogue of the third-party tool releases that soldr's CI
consumes.

```
/                              # root
├── manifest.json              # top-level index: tools -> subdir
├── zccache/manifest.json      # one per tool: assets + public download URLs
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
   **once** (authenticated, 5000 req/hour) and writes the resolved
   `browser_download_url` for every asset into the per-tool files here.
2. The workflow commits any diff back to this branch.
3. Consumer workflows fetch from
   `https://raw.githubusercontent.com/zackees/soldr/manifest/<path>` — that
   URL is CDN-served and **not** subject to the API rate limit.

Per-tool files only change when upstream actually changes (the script uses
content equality, not timestamps). `git log` here is the source of truth for
"when did this tool's release URL last change."

## Schema

Top-level (`/manifest.json`) — index only, no asset URLs:

```json
{
  "schema_version": 1,
  "tools": {
    "<tool-name>": {
      "path":        "<tool-name>/manifest.json",
      "version":     "1.12.9",
      "tag":         "1.12.9",
      "owner":       "zackees",
      "repo":        "zccache",
      "asset_count": 17
    }
  }
}
```

Per-tool (`<tool-name>/manifest.json`):

```json
{
  "schema_version":   1,
  "name":             "zccache",
  "owner":            "zackees",
  "repo":             "zccache",
  "version":          "1.12.9",
  "tag":              "1.12.9",
  "release_html_url": "https://github.com/.../releases/tag/1.12.9",
  "published_at":     "...",
  "assets": {
    "<asset-filename>": {
      "url":          "https://github.com/.../releases/download/<tag>/<filename>",
      "size":         12345678,
      "content_type": "application/x-gtar"
    }
  }
}
```

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
that the tree is content-addressable: a viewer can sha256 a file here and
trust it for as long as the commit stays on the branch. Rewriting history
breaks that trust.
