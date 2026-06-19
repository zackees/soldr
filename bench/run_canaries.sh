#!/usr/bin/env bash
# Runs the 6 canary benchmarks documented in issue #768 against
# `perf/fixtures/medium` and emits a single JSON file at
# `./benchmark-output/canaries.json` consumed by
# `bench/assemble_benchmark_stats.sh`.
#
# Canary sequence is chosen so each measurement starts from a known
# state. Total wall ~100s on a quiet Linux runner.
#
# Output schema (canaries.json):
#   {
#     "wall_ms": {
#       "cargo-build-medium-cold": <int>,
#       "cargo-build-medium-warm": <int>,
#       "cargo-build-medium-from-warm-zccache": <int>,
#       "cargo-check-medium-cross-verb": <int>,
#       "touch-no-change-medium-warm": <int>,
#       "worktree-share-medium-warm": <int>
#     },
#     "ran_at": "ISO-8601 UTC",
#     "soldr_version": "<output of `soldr --version`>",
#     "rustc_version": "<output of `rustc --version`>"
#   }

set -euo pipefail

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${HERE}/.." && pwd)"

OUT_DIR="${REPO_ROOT}/benchmark-output"
WORK_DIR="$(mktemp -d)"
trap 'rm -rf "${WORK_DIR}"' EXIT

mkdir -p "${OUT_DIR}"

# Single private cache for the whole canary sweep. Every canary writes to
# the same daemon so warm-cache measurements are meaningful.
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

# --- Canary 1: cargo-build-medium-cold --------------------------------
# Fresh fixture, fresh cache. Wall time = the maximum-cost baseline.

cd "${PROJECT_A}"
t0="$(now_ms)"
soldr cargo build --release >/dev/null 2>&1
ms_cold="$(elapsed_ms "${t0}")"

# --- Canary 2: cargo-build-medium-warm --------------------------------
# Immediate repeat. target/ intact, nothing to rebuild. Wall time =
# cargo's freshness fast-path.

t0="$(now_ms)"
soldr cargo build --release >/dev/null 2>&1
ms_warm="$(elapsed_ms "${t0}")"

# --- Canary 4: cargo-check-medium-cross-verb --------------------------
# Run check AFTER warm build but BEFORE any other state change. This
# pins #758 / zccache#776 — today every unit re-emits metadata because
# the rustc --emit flag differs from what build used.

t0="$(now_ms)"
soldr cargo check --release >/dev/null 2>&1
ms_check_cross_verb="$(elapsed_ms "${t0}")"

# --- Canary 5: touch-no-change-medium-warm ----------------------------
# Bump mtimes on every source file (content unchanged), wipe target/,
# rebuild. Cargo must invoke rustc; zccache should hit on every unit
# because the content hash is unchanged.

find "${PROJECT_A}" -name '*.rs' -exec touch {} +
find "${PROJECT_A}" -name 'Cargo.toml' -exec touch {} +
find "${PROJECT_A}" -name 'Cargo.lock' -exec touch {} +
cargo clean >/dev/null 2>&1
t0="$(now_ms)"
soldr cargo build --release >/dev/null 2>&1
ms_touch="$(elapsed_ms "${t0}")"

# --- Canary 3: cargo-build-medium-from-warm-zccache -------------------
# cargo clean only (no mtime bump). Cargo recompiles from scratch;
# zccache should hit on every unit. Distinct from canary 5 because
# cargo's fingerprint reason for invoking rustc is different here
# (cleaned target/ vs. dirty mtimes).

cargo clean >/dev/null 2>&1
t0="$(now_ms)"
soldr cargo build --release >/dev/null 2>&1
ms_from_warm="$(elapsed_ms "${t0}")"

# --- Canary 6: worktree-share-medium-warm -----------------------------
# Second extraction of the SAME fixture into a sibling dir, same
# SOLDR_CACHE_DIR. zccache should hit via path-remap on every unit.

FIX_B="${WORK_DIR}/medium-B"
mkdir -p "${FIX_B}"
bash "${REPO_ROOT}/perf/lib/extract.sh" medium "${FIX_B}"
PROJECT_B="${FIX_B}/medium"
cd "${PROJECT_B}"
t0="$(now_ms)"
soldr cargo build --release >/dev/null 2>&1
ms_worktree_share="$(elapsed_ms "${t0}")"

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
