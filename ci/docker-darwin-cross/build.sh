#!/usr/bin/env bash
# Host-side driver for the darwin-cross harness.
#
#   ./ci/docker-darwin-cross/build.sh                       # default: x86_64-apple-darwin debug
#   ./ci/docker-darwin-cross/build.sh aarch64-apple-darwin
#   ./ci/docker-darwin-cross/build.sh x86_64-apple-darwin --release
#
# First invocation builds the image (~5-10 min including SDK fetch).
# Subsequent runs are 2-3 min per cargo iteration (target/ persists
# under the mounted source tree, so incremental compile applies).

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
IMAGE_TAG="soldr-darwin-cross:latest"

TARGET="${1:-x86_64-apple-darwin}"
shift || true

# (Re)build the image if missing.
if ! docker image inspect "$IMAGE_TAG" >/dev/null 2>&1; then
    echo "[build.sh] image $IMAGE_TAG not present; building..."
    docker build \
        -f "$REPO_ROOT/ci/docker-darwin-cross/Dockerfile" \
        -t "$IMAGE_TAG" \
        "$REPO_ROOT"
fi

echo "[build.sh] cross-compiling soldr-cli for $TARGET ..."
docker run --rm \
    -v "$REPO_ROOT:/src" \
    -w /src \
    "$IMAGE_TAG" \
    /opt/build-soldr-for-darwin.sh "$TARGET" build "$@"

OUTBIN="target/$TARGET/${1:-debug}/soldr"
# If --release was passed, the binary lives under release/.
case "$*" in *--release*) OUTBIN="target/$TARGET/release/soldr" ;; esac

if [ -f "$REPO_ROOT/$OUTBIN" ]; then
    echo "[build.sh] OK — $(file "$REPO_ROOT/$OUTBIN")"
else
    echo "[build.sh] WARN: expected output at $OUTBIN — did the build succeed?"
fi
