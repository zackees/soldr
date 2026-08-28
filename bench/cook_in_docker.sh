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
# ## Volume layout (issue #593)
#
# Three named volumes — explicit lifetimes, no host bind mounts of state
# directories:
#
# * `cook-soldr-home` → `/root/.soldr` (test harness state). Wiped per
#   run for determinism — the cook-index integration tests assert
#   against a freshly-empty `~/.soldr/`.
# * `soldr-perf-target` → `/work/target` (cargo build state). Persistent
#   across runs so cargo's mtime-based fingerprint check actually
#   succeeds. Without this, Windows + Docker Desktop's WSL2 9P layer
#   rewrites file mtimes on every container start and cargo rebuilds
#   the workspace (~6 min on a fresh 21-crate workspace, vs ~1 s when
#   warm).
# * `soldr-perf-cargo-home` → `/root/.cargo` (cargo registry index +
#   downloaded crates). Persistent across runs so the registry index
#   isn't re-fetched on every container start.
#
# Required guarantee per meta #579: the host's `~/.soldr/` directory
# must be byte-identical before and after this script runs. The
# `cook-soldr-home` volume mount makes that automatic — the host's
# actual `~/.soldr/` is never bind-mounted.
#
# Migration after upgrading to this script: the old host-side `target/`
# directory under the repo root becomes orphaned (cargo writes into the
# named volume instead). Reclaim disk with `rm -rf target/`. Wipe the
# warm volumes explicitly with:
#
#   docker volume rm soldr-perf-target soldr-perf-cargo-home
#
# if the fingerprint state ever gets corrupted.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

IMAGE="soldr-cook-dev"
HARNESS_VOLUME="cook-soldr-home"
PERSIST_TARGET_VOLUME="soldr-perf-target"
PERSIST_CARGO_VOLUME="soldr-perf-cargo-home"

# Build (cached after the first run).
docker build \
    -f docker/cook-shared-cache/Dockerfile \
    -t "$IMAGE" \
    "$REPO_ROOT"

# Default cargo invocation: run the cook_index integration tests gated
# on the Docker harness marker.
if [ "$#" -eq 0 ]; then
    set -- cargo test --workspace --test cook_dylint -- --include-ignored daemon_cook_index::
fi

# Per-run wipe of `cook-soldr-home` keeps the harness-gated tests
# deterministic. The persistent `soldr-perf-*` volumes are NEVER wiped
# here — that's the entire point of this script's #593 design.
docker volume rm --force "$HARNESS_VOLUME" >/dev/null 2>&1 || true

exec docker run \
    --rm \
    --init \
    -v "$REPO_ROOT:/work" \
    -v "$HARNESS_VOLUME:/root/.soldr" \
    -v "$PERSIST_TARGET_VOLUME:/work/target" \
    -v "$PERSIST_CARGO_VOLUME:/root/.cargo" \
    -e SOLDR_COOK_DOCKER_HARNESS=1 \
    -w /work \
    "$IMAGE" \
    "$@"
