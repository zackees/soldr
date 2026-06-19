#!/usr/bin/env bash
# Publishes the assembled benchmark-stats dir to the `benchmark-stats`
# branch, bounded to a rolling 50 commits via shallow-clone + force-push.
#
# Inputs:
#   ENV REPO_FULL  (owner/repo)
#   ENV GIT_SHA    (the main-commit being recorded)
#   ENV GH_TOKEN   (write access)
#   ./benchmark-stats/ (assembled by assemble_benchmark_stats.sh)
#
# Mechanism:
#   1. Try to shallow-clone the existing branch with depth=49.
#   2. If the branch doesn't exist or is shallower than 49, fall back to
#      a fresh init (first run or post-history-loss recovery).
#   3. Overlay the new content, commit, force-push.
#   4. The remote ends up with exactly the local clone's history. When
#      we shallow-clone depth=49 and add commit #50, the remote keeps
#      50 commits; older commits become unreferenced and GitHub
#      garbage-collects them.

set -euo pipefail

: "${REPO_FULL:?missing}"
: "${GIT_SHA:?missing}"
: "${GH_TOKEN:?missing}"

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${HERE}/.." && pwd)"

SOURCE_DIR="${REPO_ROOT}/benchmark-stats"
if [[ ! -d "${SOURCE_DIR}" ]]; then
    echo "publish: ${SOURCE_DIR} not found; did assemble_benchmark_stats.sh complete?" >&2
    exit 1
fi

REMOTE_URL="https://x-access-token:${GH_TOKEN}@github.com/${REPO_FULL}.git"
PUBLISH_DIR="$(mktemp -d)"
trap 'rm -rf "${PUBLISH_DIR}"' EXIT

# --- Clone or init the branch ----------------------------------------

if git clone --depth=49 --branch=benchmark-stats "${REMOTE_URL}" "${PUBLISH_DIR}" 2>/dev/null; then
    echo "publish: shallow-cloned existing benchmark-stats branch" >&2
elif git clone --branch=benchmark-stats "${REMOTE_URL}" "${PUBLISH_DIR}" 2>/dev/null; then
    echo "publish: cloned existing benchmark-stats branch (full, was shallower than 49 commits)" >&2
else
    echo "publish: benchmark-stats branch not present yet; initializing fresh" >&2
    git -C "${PUBLISH_DIR}" init
    git -C "${PUBLISH_DIR}" checkout -b benchmark-stats
fi

# --- Overlay new content ---------------------------------------------

# Clear everything tracked / present in the publish dir and replace
# with our freshly-assembled set. This is the only safe way to ensure
# stale files don't linger across runs (we own the entire branch).
find "${PUBLISH_DIR}" -mindepth 1 -maxdepth 1 -not -name '.git' -exec rm -rf {} +
cp -a "${SOURCE_DIR}/." "${PUBLISH_DIR}/"

# --- Commit + force-push ---------------------------------------------

git -C "${PUBLISH_DIR}" config user.name "github-actions[bot]"
git -C "${PUBLISH_DIR}" config user.email "41898282+github-actions[bot]@users.noreply.github.com"

git -C "${PUBLISH_DIR}" add -A

if git -C "${PUBLISH_DIR}" diff --cached --quiet; then
    echo "publish: no content changes vs. existing tip; skipping commit" >&2
    exit 0
fi

git -C "${PUBLISH_DIR}" commit -m "chore: publish benchmark stats for ${GIT_SHA}"

# Set the remote if we initialized fresh (clone path already has it).
if ! git -C "${PUBLISH_DIR}" remote get-url origin >/dev/null 2>&1; then
    git -C "${PUBLISH_DIR}" remote add origin "${REMOTE_URL}"
fi

git -C "${PUBLISH_DIR}" push --force origin benchmark-stats

echo "publish: pushed benchmark-stats" >&2
