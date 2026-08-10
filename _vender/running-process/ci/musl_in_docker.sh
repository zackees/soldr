#!/usr/bin/env bash
# ci/musl_in_docker.sh — build and run the musl x86 unit-test target
# inside a Docker container, mirroring soldr's bench/cook_in_docker.sh
# pattern.
#
# Defaults to `soldr cargo nextest run --workspace --exclude
# running-process-py --features client --target x86_64-unknown-linux-musl`
# — the exact CI invocation for the `x86-musl / unit-test` lane.
#
# Usage:
#   ci/musl_in_docker.sh                   # default: musl nextest run
#   ci/musl_in_docker.sh soldr cargo test  # any other cargo invocation
#   ci/musl_in_docker.sh shell             # interactive bash
#
# Volume layout (issue #513): three named volumes — no host bind
# mounts of state directories. See docker/musl-unit-test/README.md
# for the full pattern.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

IMAGE="running-process-musl-unit-test"
TARGET_VOLUME="rp-musl-target"
CARGO_HOME_VOLUME="rp-musl-cargo-home"
SOLDR_HOME_VOLUME="rp-musl-soldr-home"

# Build (cached after the first run).
docker build \
    -f docker/musl-unit-test/Dockerfile \
    -t "$IMAGE" \
    "$REPO_ROOT/docker/musl-unit-test"

# Default invocation mirrors the failing CI step exactly.
if [ "$#" -eq 0 ]; then
    set -- soldr cargo nextest run \
        --workspace --exclude running-process-py \
        --features client \
        --target x86_64-unknown-linux-musl
fi

# Interactive shell shorthand. Only adds `-it` when running an
# interactive shell — git-bash without a TTY chokes on `-it` for
# non-interactive cargo runs.
INTERACTIVE_FLAGS=()
if [ "$1" = "shell" ]; then
    set -- bash
    INTERACTIVE_FLAGS=(-it)
fi

# MSYS_NO_PATHCONV=1 keeps git-bash from mangling Linux paths in -v
# arguments on Windows. No-op on Linux/macOS.
exec env MSYS_NO_PATHCONV=1 docker run \
    --rm \
    --init \
    "${INTERACTIVE_FLAGS[@]}" \
    -v "$REPO_ROOT:/work" \
    -v "$TARGET_VOLUME:/work/target" \
    -v "$CARGO_HOME_VOLUME:/root/.cargo" \
    -v "$SOLDR_HOME_VOLUME:/root/.soldr" \
    -w /work \
    "$IMAGE" \
    "$@"
