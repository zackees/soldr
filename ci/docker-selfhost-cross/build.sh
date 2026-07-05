#!/usr/bin/env bash
# Host-side driver for the self-hosting cross-compile proof (soldr#1309).
#
# Usage:
#   ./ci/docker-selfhost-cross/build.sh                        # aarch64-apple-darwin
#   ./ci/docker-selfhost-cross/build.sh x86_64-pc-windows-msvc
#   ./ci/docker-selfhost-cross/build.sh --rebuild-image
#   ./ci/docker-selfhost-cross/build.sh --clean-volumes
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
IMAGE_TAG="soldr-selfhost-cross:latest"
export MSYS_NO_PATHCONV=1
export MSYS2_ARG_CONV_EXCL='*'

# On a Windows host (Git-Bash / MSYS) the Docker daemon needs a
# Windows-style context/mount path (`C:/Users/...`), not the MSYS
# `/c/Users/...` form. `cygpath -m` yields the forward-slash Windows
# path Docker accepts. No-op on Linux/macOS where cygpath is absent.
if command -v cygpath >/dev/null 2>&1; then
    REPO_ROOT="$(cygpath -m "$REPO_ROOT")"
fi

VOL_CARGO_REGISTRY="soldr-selfhost-cargo-registry"
VOL_TARGET="soldr-selfhost-target"

target=""
mode="build"
for arg in "$@"; do
    case "$arg" in
        --rebuild-image)  mode="rebuild" ;;
        --clean-volumes)  mode="clean-volumes" ;;
        --shell|-s)       mode="shell" ;;
        --*)              ;;
        *) [ -z "$target" ] && target="$arg" ;;
    esac
done
: "${target:=aarch64-apple-darwin}"

case "$mode" in
    clean-volumes)
        docker volume rm "$VOL_CARGO_REGISTRY" "$VOL_TARGET" 2>/dev/null || true
        exit 0 ;;
    rebuild)
        docker build --no-cache -f "$REPO_ROOT/ci/docker-selfhost-cross/Dockerfile" \
            -t "$IMAGE_TAG" "$REPO_ROOT"
        exit 0 ;;
esac

if ! docker image inspect "$IMAGE_TAG" >/dev/null 2>&1; then
    echo "[build.sh] building image $IMAGE_TAG (one-time)..."
    docker build -f "$REPO_ROOT/ci/docker-selfhost-cross/Dockerfile" -t "$IMAGE_TAG" "$REPO_ROOT"
fi
for vol in "$VOL_CARGO_REGISTRY" "$VOL_TARGET"; do
    docker volume inspect "$vol" >/dev/null 2>&1 || docker volume create "$vol" >/dev/null
done

declare -a run_args=(
    --rm
    -v "$REPO_ROOT:/src"
    -w /src
    -v "${VOL_CARGO_REGISTRY}:/root/.cargo/registry"
    -v "${VOL_TARGET}:/src/target"
)

if [ "$mode" = "shell" ]; then
    docker run "${run_args[@]}" -it --entrypoint bash "$IMAGE_TAG"
    exit 0
fi

echo "[build.sh] self-hosting cross build for $target ..."
docker run "${run_args[@]}" "$IMAGE_TAG" "$target"
