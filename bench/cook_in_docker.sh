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
# directories. Every name is suffixed with this checkout's shared git
# root (see "Per-checkout isolation" below), shown here as <root>:
#
# * `cook-soldr-home-<root>` → `/root/.soldr` (test harness state).
#   Wiped per run for determinism — the cook-index integration tests
#   assert against a freshly-empty `~/.soldr/`.
# * `soldr-perf-target-<root>` → `/work/target` (cargo build state).
#   Persistent across runs so cargo's mtime-based fingerprint check
#   actually succeeds. Without this, Windows + Docker Desktop's WSL2 9P
#   layer rewrites file mtimes on every container start and cargo
#   rebuilds the workspace (~6 min on a fresh 21-crate workspace, vs
#   ~1 s when warm).
# * `soldr-perf-cargo-home-<root>` → `/root/.cargo` (cargo registry
#   index + downloaded crates). Persistent across runs so the registry
#   index isn't re-fetched on every container start.
#
# ## Per-checkout isolation
#
# The volume names used to be machine-wide, so sibling checkouts
# (soldr, soldr2, soldr3) shared them. Two ways that bit:
#
# * The per-run `docker volume rm --force cook-soldr-home` below would
#   yank the harness volume out from under a run in ANOTHER checkout.
# * All roots shared one cargo target across different branches, so
#   each invalidated the others' fingerprints — exactly the rebuild
#   this script exists to avoid.
#
# Names are now derived from the shared git root, so each checkout gets
# its own set. Linked worktrees below a root deliberately SHARE that
# root's volumes: `git rev-parse --git-common-dir` resolves a worktree
# to its owning checkout, matching ci/perf_local.py's behavior.
#
# Required guarantee per meta #579: the host's `~/.soldr/` directory
# must be byte-identical before and after this script runs. The
# `cook-soldr-home-<root>` volume mount makes that automatic — the
# host's actual `~/.soldr/` is never bind-mounted.
#
# Migration after upgrading to this script: the old host-side `target/`
# directory under the repo root becomes orphaned (cargo writes into the
# named volume instead). Reclaim disk with `rm -rf target/`.
#
# Print the resolved names without running anything:
#
#   SOLDR_COOK_PRINT_PLAN=1 bench/cook_in_docker.sh
#
# and wipe this checkout's warm volumes explicitly with the two
# `soldr-perf-*` names it prints, if fingerprint state gets corrupted.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# The checkout that owns this tree. A linked worktree resolves to its
# owning checkout, so worktrees share their root's warm volumes instead
# of each cold-building into their own.
SOURCE_ROOT="$REPO_ROOT"
if common_dir="$(git rev-parse --path-format=absolute --git-common-dir 2>/dev/null)"; then
    if [ "$(basename "$common_dir")" = ".git" ]; then
        SOURCE_ROOT="$(cd "$(dirname "$common_dir")" && pwd)"
    fi
fi

# Case-folded because Windows paths are case-insensitive: `...\Soldr2`
# and `...\soldr2` are one checkout and must not get two volume sets.
root_hash="$(printf '%s' "$SOURCE_ROOT" | tr '[:upper:]' '[:lower:]' | sha256sum | cut -c1-8)"
# Readable leaf name, sanitized to what Docker accepts in a volume name.
root_slug="$(basename "$SOURCE_ROOT" | tr '[:upper:]' '[:lower:]' | sed -e 's/[^a-z0-9]/-/g' -e 's/^-*//' -e 's/-*$//' | cut -c1-24)"
[ -n "$root_slug" ] || root_slug="repo"
ROOT_SUFFIX="${root_slug}-${root_hash}"

IMAGE="soldr-cook-dev"
HARNESS_VOLUME="cook-soldr-home-${ROOT_SUFFIX}"
PERSIST_TARGET_VOLUME="soldr-perf-target-${ROOT_SUFFIX}"
PERSIST_CARGO_VOLUME="soldr-perf-cargo-home-${ROOT_SUFFIX}"

if [ "${SOLDR_COOK_PRINT_PLAN:-}" = "1" ]; then
    printf 'source_root=%s\n' "$SOURCE_ROOT"
    printf 'harness_volume=%s\n' "$HARNESS_VOLUME"
    printf 'target_volume=%s\n' "$PERSIST_TARGET_VOLUME"
    printf 'cargo_volume=%s\n' "$PERSIST_CARGO_VOLUME"
    exit 0
fi

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

# Per-run wipe of this checkout's harness volume keeps the
# harness-gated tests deterministic. It is per-root, so it can no longer
# destroy a sibling checkout's in-flight run. The persistent
# `soldr-perf-*` volumes are NEVER wiped here — that's the entire point
# of this script's #593 design.
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
