#!/usr/bin/env bash
# bench/cook_in_docker.sh — build and test the cross-repo cook cache
# feature inside a Docker container so the host's `~/.soldr/` singleton
# is never mutated. See docker/cook-shared-cache/Dockerfile for context.
#
# Defaults to running the cook-index integration tests added in PR 1
# (#576 under meta #579). Pass a custom cargo invocation as arguments to
# override.
#
# Usage:
#   bench/cook_in_docker.sh                       # default: cook-index tests
#   bench/cook_in_docker.sh cargo test --workspace   # full workspace test
#   bench/cook_in_docker.sh cargo clippy --workspace # clippy lint
#
# Required guarantee per meta #579: the host's `~/.soldr/` directory
# must be byte-identical before and after this script runs. We mount a
# named volume (cook-soldr-home) inside the container at /root/.soldr;
# the host's actual `~/.soldr/` is never bind-mounted.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

IMAGE="soldr-cook-dev"
VOLUME="cook-soldr-home"

# Build (cached after the first run).
docker build \
    -f docker/cook-shared-cache/Dockerfile \
    -t "$IMAGE" \
    "$REPO_ROOT"

# Default cargo invocation: run the cook_index integration tests gated
# on the Docker harness marker.
if [ "$#" -eq 0 ]; then
    set -- cargo test --workspace --test daemon_cook_index -- --include-ignored
fi

# Fresh `~/.soldr/` each time keeps tests deterministic. Remove the
# volume so the next run starts from empty state; comment out the
# volume rm if you want to debug across runs.
docker volume rm --force "$VOLUME" >/dev/null 2>&1 || true

exec docker run \
    --rm \
    --init \
    -v "$REPO_ROOT:/work" \
    -v "$VOLUME:/root/.soldr" \
    -e SOLDR_COOK_DOCKER_HARNESS=1 \
    -w /work \
    "$IMAGE" \
    "$@"
