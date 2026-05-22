#!/usr/bin/env bash
# Convenience wrapper for the zackees/soldr#424 reproduction harness:
# builds the image and runs the container in one shot.
#
# Run from the soldr repo root:
#   bash bench/docker/update-zccache-pin-honored/run.sh
#
# Pass-through flags after `--` go to `docker run`. Useful for
# interactive debugging: `bash run.sh -- -it --entrypoint /bin/bash`.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
IMAGE_TAG="${SOLDR_PIN_REPRO_IMAGE_TAG:-soldr-pin-repro:local}"
DOCKERFILE="$REPO_ROOT/bench/docker/update-zccache-pin-honored/Dockerfile"

extra_run_args=()
seen_dash_dash=0
for arg in "$@"; do
    if [ "$seen_dash_dash" -eq 1 ]; then
        extra_run_args+=("$arg")
    elif [ "$arg" = "--" ]; then
        seen_dash_dash=1
    fi
done

echo "==> building $IMAGE_TAG"
docker build -t "$IMAGE_TAG" -f "$DOCKERFILE" "$REPO_ROOT"

echo "==> running $IMAGE_TAG"
docker run --rm "${extra_run_args[@]}" "$IMAGE_TAG"
