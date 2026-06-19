#!/usr/bin/env bash
# Runs the 6 canary benchmarks documented in issue #768 against
# `perf/fixtures/medium` and emits a single JSON file at
# `./benchmark-output/canaries.json` consumed by
# `bench/assemble_benchmark_stats.sh`.
#
# Canary sequence is chosen so each measurement starts from a known
# state. Total wall ~100s on a quiet Linux runner.
#
# Defensive: every canary runs with `set +e` around it via the
# `measure()` helper. A canary failure logs stderr to the workflow
# log and records 0 ms so the publish step still runs and the README
# image link resolves with whatever data we have. The PNG renderer
# treats 0 ms as a missing data point.
#
# Output schema (canaries.json):
#   {
#     "wall_ms": { "<canary-name>": <int>, ... },
#     "ran_at":         "ISO-8601 UTC",
#     "soldr_version":  "<output of soldr --version>",
#     "rustc_version":  "<output of rustc --version>"
#   }

set -uo pipefail

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${HERE}/.." && pwd)"

OUT_DIR="${REPO_ROOT}/benchmark-output"
WORK_DIR="$(mktemp -d)"
trap 'rm -rf "${WORK_DIR}"' EXIT

mkdir -p "${OUT_DIR}"

# Issue #778: setup-soldr exports a fleet of env vars that pin soldr's
# behavior to the soldr workspace it was set up for
# (`<runner>/soldr/soldr/soldr`). When the canary script cd's to the
# medium fixture, those env vars cross-talk:
#   - ZCCACHE_CACHE_DIR is pinned to setup-soldr's tempdir, but we
#     override SOLDR_CACHE_DIR below — soldr passes our SOLDR_CACHE_DIR
#     to zccache while zccache sees the old ZCCACHE_CACHE_DIR and the
#     two disagree.
#   - SOLDR_TARGET_CACHE_DIR / _BUNDLE_DIR / _REGISTRY_RECORDED point
#     at paths under setup-soldr's tempdir; the medium fixture has its
#     own target/ in a totally unrelated place.
#   - SETUP_SOLDR_* / SOLDR_BUILD_CACHE_MODE / SOLDR_TARGET_CACHE_MODE
#     are setup-soldr bookkeeping; safe to strip.
#
# KEEP: PATH (soldr lives there), RUSTUP_HOME / RUSTUP_TOOLCHAIN /
# CARGO_HOME (toolchain), SOLDR_BINARY (alternate binary resolution),
# SOLDR_LINKER, ZCCACHE_COMPILE_PRIORITY (useful, not workspace-pinned).
#
# This keeps setup-soldr's fast-build benefits for the soldr binary
# itself (which was built earlier in the workflow's setup phase) while
# letting the canaries run as if from a clean shell.
unset ZCCACHE_CACHE_DIR \
      SOLDR_TARGET_CACHE_DIR \
      SOLDR_TARGET_CACHE_BUNDLE_DIR \
      SOLDR_TARGET_CACHE_MODE \
      SOLDR_TARGET_CACHE_PROFILE \
      SOLDR_TARGET_CACHE_BACKEND \
      SOLDR_TARGET_CACHE_COMPRESS \
      SOLDR_TARGET_CACHE_COMPRESS_LEVEL \
      SOLDR_TARGET_REGISTRY_RECORDED \
      SOLDR_BUILD_CACHE_MODE \
      SETUP_SOLDR_BUILD_CACHE_MODE

# Single private cache for the whole canary sweep. Every canary writes
# to the same daemon so warm-cache measurements are meaningful.
export SOLDR_CACHE_DIR="${WORK_DIR}/cache"
mkdir -p "${SOLDR_CACHE_DIR}"

# Extract the medium fixture into a primary build dir.
FIX_A="${WORK_DIR}/medium-A"
mkdir -p "${FIX_A}"
bash "${REPO_ROOT}/perf/lib/extract.sh" medium "${FIX_A}"
PROJECT_A="${FIX_A}/medium"

now_ms() { date +%s%3N; }

elapsed_ms() {
    local start="$1"
    local end
    end="$(now_ms)"
    echo "$(( end - start ))"
}

# measure NAME CMD...
# Runs CMD with stderr passed through to the workflow log, recording
# the wall time. On failure: log a one-line marker and record 0 ms
# so the rest of the canary sweep continues and the publish proceeds.
measure() {
    local name="$1"
    shift
    local t0 rc
    echo "::group::canary $name" >&2
    echo "+ $*" >&2
    t0="$(now_ms)"
    set +e
    "$@"
    rc=$?
    set -e
    local elapsed
    elapsed="$(elapsed_ms "${t0}")"
    if (( rc == 0 )); then
        echo "canary $name: ok (${elapsed} ms)" >&2
        echo "::endgroup::" >&2
        echo "${elapsed}"
    else
        echo "canary $name: FAILED (rc=${rc}, ${elapsed} ms) — recording 0" >&2
        echo "::endgroup::" >&2
        echo "0"
    fi
}

# --- Canary 1: cargo-build-medium-cold --------------------------------
# Fresh fixture, fresh cache. Wall time = the maximum-cost baseline.

cd "${PROJECT_A}"
ms_cold="$(measure cargo-build-medium-cold soldr cargo build --release)"

# --- Canary 2: cargo-build-medium-warm --------------------------------
# Immediate repeat. target/ intact, nothing to rebuild. Wall time =
# cargo's freshness fast-path.

ms_warm="$(measure cargo-build-medium-warm soldr cargo build --release)"

# --- Canary 4: cargo-check-medium-cross-verb --------------------------
# Run check AFTER warm build but BEFORE any other state change. This
# pins #758 / zccache#776 — today every unit re-emits metadata because
# the rustc --emit flag differs from what build used.

ms_check_cross_verb="$(measure cargo-check-medium-cross-verb soldr cargo check --release)"

# --- Canary 5: touch-no-change-medium-warm ----------------------------
# Bump mtimes on every source file (content unchanged), wipe target/,
# rebuild. Cargo must invoke rustc; zccache should hit on every unit
# because the content hash is unchanged.

find "${PROJECT_A}" -name '*.rs' -exec touch {} + || true
find "${PROJECT_A}" -name 'Cargo.toml' -exec touch {} + || true
find "${PROJECT_A}" -name 'Cargo.lock' -exec touch {} + || true
cargo clean || true
ms_touch="$(measure touch-no-change-medium-warm soldr cargo build --release)"

# --- Canary 3: cargo-build-medium-from-warm-zccache -------------------
# cargo clean only (no mtime bump). Cargo recompiles from scratch;
# zccache should hit on every unit. Distinct from canary 5 because
# cargo's fingerprint reason for invoking rustc is different here
# (cleaned target/ vs. dirty mtimes).

cargo clean || true
ms_from_warm="$(measure cargo-build-medium-from-warm-zccache soldr cargo build --release)"

# --- Canary 6: worktree-share-medium-warm -----------------------------
# Second extraction of the SAME fixture into a sibling dir, same
# SOLDR_CACHE_DIR. zccache should hit via path-remap on every unit.

FIX_B="${WORK_DIR}/medium-B"
mkdir -p "${FIX_B}"
bash "${REPO_ROOT}/perf/lib/extract.sh" medium "${FIX_B}" || true
PROJECT_B="${FIX_B}/medium"
cd "${PROJECT_B}"
ms_worktree_share="$(measure worktree-share-medium-warm soldr cargo build --release)"

# --- Emit -------------------------------------------------------------

ran_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
soldr_version="$(soldr --version 2>/dev/null || echo unknown)"
rustc_version="$(rustc --version 2>/dev/null || echo unknown)"

jq -n \
    --arg ran_at "${ran_at}" \
    --arg soldr_version "${soldr_version}" \
    --arg rustc_version "${rustc_version}" \
    --argjson ms_cold "${ms_cold}" \
    --argjson ms_warm "${ms_warm}" \
    --argjson ms_check "${ms_check_cross_verb}" \
    --argjson ms_touch "${ms_touch}" \
    --argjson ms_from_warm "${ms_from_warm}" \
    --argjson ms_worktree "${ms_worktree_share}" \
    '{
        wall_ms: {
            "cargo-build-medium-cold": $ms_cold,
            "cargo-build-medium-warm": $ms_warm,
            "cargo-build-medium-from-warm-zccache": $ms_from_warm,
            "cargo-check-medium-cross-verb": $ms_check,
            "touch-no-change-medium-warm": $ms_touch,
            "worktree-share-medium-warm": $ms_worktree
        },
        ran_at: $ran_at,
        soldr_version: $soldr_version,
        rustc_version: $rustc_version
    }' >"${OUT_DIR}/canaries.json"

echo "canaries.json written to ${OUT_DIR}/canaries.json" >&2
cat "${OUT_DIR}/canaries.json" >&2
