#!/usr/bin/env bash
# Resolve a single asset URL from the public `manifest` branch and curl
# it to a local path. Replaces the per-lane GitHub Releases API lookup
# the cross-compile workflow used to do (which 403'd on the
# unauthenticated 60-req/hour quota when 7 parallel lanes hammered it).
#
# Usage:
#     fetch_manifest_asset.sh <tool> <tag-or-marker> <asset-name> <output-path>
#
#   tool          — directory name on the manifest branch (zccache,
#                   cargo-zigbuild, cargo-xwin, crgx, cargo-chef).
#   tag-or-marker — exact tag string (e.g. "1.12.9", "v0.23.0"), or
#                   the literal "latest" / "pinned" — which resolve
#                   to manifest["latest"] / manifest["pinned"]
#                   respectively.
#   asset-name    — release asset filename to download.
#   output-path   — where to save the asset on disk.
#
# Both the per-tool manifest fetch and the asset download go through
# `raw.githubusercontent.com` (CDN-served, not API-rate-limited).
#
# The branch ref is overridable for testing via
# `SOLDR_MANIFEST_BRANCH_REF` (default: `manifest`); the owner/repo is
# overridable via `SOLDR_MANIFEST_BRANCH_REPO` (default:
# `zackees/soldr`). In CI both stay at defaults.

set -euo pipefail

tool="${1:?tool argument required}"
tag_or_marker="${2:?tag-or-marker argument required}"
asset_name="${3:?asset-name argument required}"
output_path="${4:?output-path argument required}"

repo="${SOLDR_MANIFEST_BRANCH_REPO:-zackees/soldr}"
ref="${SOLDR_MANIFEST_BRANCH_REF:-manifest}"
base="https://raw.githubusercontent.com/${repo}/${ref}"
manifest_url="${base}/${tool}/manifest.json"

manifest_tmp="$(mktemp --tmpdir manifest.XXXXXX.json)"
trap 'rm -f "$manifest_tmp"' EXIT

curl --fail --location --silent --show-error \
    --retry 6 --retry-delay 5 --retry-all-errors \
    --output "$manifest_tmp" "$manifest_url"

case "$tag_or_marker" in
    latest|pinned)
        tag=$(jq -er --arg key "$tag_or_marker" '.[$key]' "$manifest_tmp")
        ;;
    *)
        tag="$tag_or_marker"
        ;;
esac

asset_url=$(jq -er --arg t "$tag" --arg a "$asset_name" \
    '.releases[$t].assets[$a].url' "$manifest_tmp")

echo "manifest: ${tool} @ ${tag} -> ${asset_name}"
echo "         ${asset_url}"

mkdir -p "$(dirname "$output_path")"
curl --fail --location \
    --retry 6 --retry-delay 5 --retry-all-errors \
    --output "$output_path" "$asset_url"
