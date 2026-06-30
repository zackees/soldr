#!/usr/bin/env bash
# Host-side driver for the darwin-cross harness.
#
# First invocation builds the image (~5-10 min including SDK fetch).
# Subsequent invocations bind-mount the source tree and use named
# volumes for the cargo registry + target dir so iteration is 30s-2min
# instead of a full from-scratch download+compile each time.
#
# Usage:
#   ./ci/docker-darwin-cross/build.sh                       # x86_64-apple-darwin debug
#   ./ci/docker-darwin-cross/build.sh aarch64-apple-darwin  # aarch64-apple-darwin
#   ./ci/docker-darwin-cross/build.sh --shell               # interactive bash
#   ./ci/docker-darwin-cross/build.sh --rebuild-image       # force rebuild docker image
#   ./ci/docker-darwin-cross/build.sh --clean-volumes       # nuke cargo+target volumes (cold start next run)

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
IMAGE_TAG="soldr-darwin-cross:latest"

# Prevent MSYS / Git-Bash on Windows from translating container-side
# paths (`/src`, `-w /src`) into host Windows paths
# (`C:/Program Files/Git/src`). No effect on Linux/macOS bash.
export MSYS_NO_PATHCONV=1
export MSYS2_ARG_CONV_EXCL='*'

# Named volumes survive `docker run --rm` so cargo's compiled deps
# persist between iterations. ~/.cargo/registry caches the crates.io
# downloads. target/$triple/ caches the compiled object files.
VOL_CARGO_REGISTRY="soldr-darwin-cargo-registry"
VOL_TARGET="soldr-darwin-target"

# Argument handling — keep it simple. First non-flag arg is the target
# triple; everything else passes through to cargo.
target=""
extra_args=()
mode="build"
for arg in "$@"; do
    case "$arg" in
        --shell|-s)             mode="shell" ;;
        --rebuild-image)        mode="rebuild" ;;
        --clean-volumes)        mode="clean-volumes" ;;
        --help|-h)              mode="help" ;;
        --*)                    extra_args+=("$arg") ;;
        *)
            if [ -z "$target" ]; then
                target="$arg"
            else
                extra_args+=("$arg")
            fi
            ;;
    esac
done
: "${target:=x86_64-apple-darwin}"

case "$mode" in
    help)
        sed -n '/^# Usage:/,/^$/p' "$0"
        exit 0
        ;;
    clean-volumes)
        echo "[build.sh] removing volumes $VOL_CARGO_REGISTRY + $VOL_TARGET"
        docker volume rm "$VOL_CARGO_REGISTRY" "$VOL_TARGET" 2>/dev/null || true
        exit 0
        ;;
    rebuild)
        echo "[build.sh] force-rebuilding image $IMAGE_TAG"
        docker build --no-cache \
            -f "$REPO_ROOT/ci/docker-darwin-cross/Dockerfile" \
            -t "$IMAGE_TAG" \
            "$REPO_ROOT"
        exit 0
        ;;
esac

# (Re)build the image if missing.
if ! docker image inspect "$IMAGE_TAG" >/dev/null 2>&1; then
    echo "[build.sh] image $IMAGE_TAG not present; building (one-time ~5-10 min)..."
    docker build \
        -f "$REPO_ROOT/ci/docker-darwin-cross/Dockerfile" \
        -t "$IMAGE_TAG" \
        "$REPO_ROOT"
fi

# Ensure named volumes exist (docker creates them lazily on first use
# but explicit creation makes the harness more inspectable).
for vol in "$VOL_CARGO_REGISTRY" "$VOL_TARGET"; do
    docker volume inspect "$vol" >/dev/null 2>&1 || docker volume create "$vol" >/dev/null
done

# Common mount/env args.
declare -a run_args=(
    --rm
    -v "$REPO_ROOT:/src"
    -w /src
    -v "${VOL_CARGO_REGISTRY}:/root/.cargo/registry"
    -v "${VOL_TARGET}:/src/target"
)

if [ "$mode" = "shell" ]; then
    echo "[build.sh] launching interactive shell in $IMAGE_TAG"
    docker run "${run_args[@]}" -it "$IMAGE_TAG" bash
    exit 0
fi

echo "[build.sh] cross-compiling soldr-cli for $target ..."
# `${arr[@]+"${arr[@]}"}` expands to nothing when arr is empty; the
# more common `${arr[@]:-}` would expand to a single empty string,
# which clap (cargo's arg parser) rejects with `error: unexpected
# argument '' found`.
docker run "${run_args[@]}" \
    "$IMAGE_TAG" \
    /opt/build-soldr-for-darwin.sh "$target" ${extra_args[@]+"${extra_args[@]}"}
