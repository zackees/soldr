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

1. A nightly workflow on `main` (`.github/workflows/refresh-manifest.yml`) runs
   `.github/scripts/build_manifest.py`, which queries the GitHub API **once per
   tool** (authenticated, 5000 req/hour, `per_page=100`) and writes the resolved
   `browser_download_url` for every asset into the per-tool files here. New
   releases get **merged into** the existing file — older tags are preserved
   as a permanent archive.
2. The workflow commits any diff back to this branch.
3. Consumer workflows fetch from
   `https://raw.githubusercontent.com/zackees/soldr/manifest/<path>` — that
   URL is CDN-served and **not** subject to the API rate limit.

Per-tool files only change when upstream actually publishes something new (the
script uses content equality, not timestamps). `git log` here is the source of
truth for "when did this tool's release set last change."

## Getting an asset by platform (dead simple, schema v3)

Each release carries a normalized `platforms` map keyed by
`<os>-<arch>[-<extra>]`. Consumers ask for the host they care about — no need
to deal with each upstream tool's idiosyncratic asset filename quirks.

```python
import json, urllib.request
m = json.loads(urllib.request.urlopen(
    "https://raw.githubusercontent.com/zackees/soldr/manifest/zccache/manifest.json"
).read())

latest_tag = m["latest"]                       # e.g. "1.12.9"
release    = m["releases"][latest_tag]
platform   = release["platforms"]["linux-x64-musl"]
url        = platform["url"]                   # public CDN download URL
filename   = platform["filename"]              # original upstream asset name
```

Or in jq for shell:

```sh
jq -er '.releases[.latest].platforms["linux-x64-musl"].url' zccache/manifest.json
```

## Platform key shape

```
os    ∈ { linux, darwin, windows }
arch  ∈ { x64, arm64, universal2 }         # 32-bit lanes are not surfaced
extra ∈ { gnu, musl, msvc, gnullvm, … }    # only when meaningful
```

Examples seen across tracked tools:

| Key | Meaning |
|---|---|
| `linux-x64-gnu` | Standard Linux x64 glibc build |
| `linux-x64-musl` | Linux x64 musl (static; runs on glibc hosts too) |
| `linux-arm64-gnu` | Linux aarch64 glibc |
| `linux-arm64-musl` | Linux aarch64 musl |
| `darwin-x64` | macOS x86_64 |
| `darwin-arm64` | macOS Apple Silicon |
| `darwin-universal2` | macOS fat binary (cargo-xwin / some tools) |
| `windows-x64-msvc` | Windows x64, official MSVC ABI |
| `windows-arm64-msvc` | Windows ARM64, official MSVC ABI |
| `windows-x64-gnu` | Windows x64, GNU ABI |
| `windows-arm64-gnullvm` | Windows ARM64, gnullvm ABI |

Modern arch names (`x64` not `x86_64`; `arm64` not `aarch64`) match the
npm/Node.js convention. **32-bit binaries (i686 / armv7) are not surfaced** —
every modern process is 64-bit, the schema doesn't need to fragment to track
them.

## Schema

Top-level (`/manifest.json`) — index, no asset URLs:

```json
{
  "schema_version": 3,
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
  "schema_version": 3,
  "name":           "zccache",
  "owner":          "zackees",
  "repo":           "zccache",
  "latest":         "1.12.9",
  "pinned":         "1.12.9",
  "tracked_tags":   ["1.12.9", "1.12.8", "..."],
  "releases": {
    "1.12.9": {
      "tag":              "1.12.9",
      "version":          "1.12.9",
      "published_at":     "...",
      "release_html_url": "...",
      "platforms": {
        "linux-x64-musl": {
          "filename": "zccache-v1.12.9-x86_64-unknown-linux-musl.tar.gz",
          "url":      "https://github.com/.../releases/download/1.12.9/...",
          "size":     12345678
        },
        "darwin-arm64":      { "filename": "...", "url": "...", "size": 0 },
        "windows-x64-msvc":  { "filename": "...", "url": "...", "size": 0 }
      },
      "assets": {
        "<original-upstream-filename>": {
          "url":          "...",
          "size":         0,
          "content_type": "application/x-gtar",
          "created_at":   "...",
          "updated_at":   "..."
        }
      }
    }
  }
}
```

`platforms` is the consumer-friendly normalized view. `assets` is the raw
upstream filename-keyed view, kept for backward compatibility and for cases
where consumers want the original filename or per-asset content-type.

## Ordering

`releases` is ordered by `published_at` descending (newest first). GitHub's
`/releases?per_page=100` already returns them in that order; the merge logic
preserves it. No client-side semver parsing.

## Debug / symbol artifacts

Files whose names contain `-debug`, `.debug`, `-sym`, or `.pdb` are tracked
in `assets` but NOT surfaced in `platforms`. Debug variants aren't the
canonical platform binary for consumer downloads.

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
