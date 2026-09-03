#!/usr/bin/env bash
# Publish the prepared macOS guest to GHCR. Run this ONCE, from a machine that
# has completed the manual bootstrap in README.md -- not from CI.
#
# Everything after this is automated: CI pulls the result (through
# ci/macos_x64_guest.py) and never installs macOS itself.
set -euo pipefail

REPO="${REPO:-zackees/soldr}"
TAG="${TAG:-ghcr.io/${REPO}/macos-x64-guest:ventura}"
STORAGE="${GUEST_STORAGE:-$HOME/.clud/docker-mac-x86/storage}"
NAME="${GUEST_NAME:-soldr-macos-x86}"

if [ ! -d "$STORAGE" ]; then
  echo "no prepared guest at $STORAGE -- complete the bootstrap first" >&2
  exit 1
fi

# The disk must be at rest, or the baked image carries a torn filesystem.
if docker ps --filter "name=^${NAME}$" --format '{{.Names}}' | grep -q .; then
  echo "stopping guest so the disk is quiescent..."
  docker stop "$NAME" >/dev/null
fi

BUILD_DIR="$(mktemp -d)"
trap 'rm -rf "$BUILD_DIR"' EXIT
cp "$(dirname "$0")/Dockerfile.guest" "$BUILD_DIR/Dockerfile"
cp -a "$STORAGE" "$BUILD_DIR/storage"

echo "baking $TAG ($(du -sh "$BUILD_DIR/storage" | cut -f1) of guest state)..."
docker build -t "$TAG" "$BUILD_DIR"
docker push "$TAG"
echo "pushed $TAG"
