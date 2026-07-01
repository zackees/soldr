#!/usr/bin/env bash
# Scenario: cold `cargo build --release`, then immediate `cargo check
# --release` against an unchanged source tree.
#
# What this measures: cross-verb cache reuse. After `build --release`
# fills zccache with rmeta + rlib for every unit, an immediate `check
# --release` SHOULD produce ~100% hits because the source, crate
# versions, and target triple are identical. Today every unit MISSES
# because zccache's cache key includes the rustc `--emit` flag, and
# cargo issues `--emit=metadata` for check vs `--emit=metadata,link`
# for build.
#
# Until zccache canonicalizes these keys (`--emit=metadata` is a strict
# subset of `--emit=metadata,link`), this row is the canary that says
# how much wall-clock the asymmetry costs.
#
# Pinned by issue #758. Soft warning today; promotes to hard fail
# once the canonicalization lands upstream.
#
# Usage: run.sh <fixture-workdir>
set -euo pipefail

if (( $# != 1 )); then
    echo "usage: run.sh <fixture-workdir>" >&2
    exit 2
fi

FIXTURE_DIR="$1"
SCENARIO="build-then-check"

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../lib/common.sh
. "${HERE}/../../lib/common.sh"

WORKDIR="$(cd -- "${FIXTURE_DIR}/.." && pwd)"
CACHE="${WORKDIR}/cache-build-then-check"
RSS_CSV="${WORKDIR}/rss-${SCENARIO}.csv"

mkdir -p "${CACHE}"

measure::start_rss_poller "${RSS_CSV}"
trap 'measure::stop_rss_poller' EXIT

# --- Cold build (populates zccache for the release profile) --------

cold_start_ms="$(measure::now_ms)"
(
    cd "${FIXTURE_DIR}"
    SOLDR_CACHE_DIR="${CACHE}" soldr cargo build --release
)
cold_elapsed_ms="$(measure::elapsed_ms "${cold_start_ms}")"

# Flush so the depgraph snapshot is durable before the cross-verb pass.
SOLDR_CACHE_DIR="${CACHE}" soldr cache flush --json >/dev/null 2>&1 || true

# Cargo's check fingerprint can short-circuit when it judges build's
# rmeta as fresh-enough — observed on Linux GHA, NOT on Windows MSVC.
# A short-circuit means zero rustc invocations and zero zccache hits or
# misses, which collapses this scenario to "MISSING stats" in the
# evaluate gate.
#
# Advance every source-file mtime (same trick as touch-no-change) so
# cargo's fingerprint check fails uniformly across platforms and cargo
# is forced to ask rustc for every unit. Content is unchanged, so this
# is still an apples-to-apples zccache test: either zccache canonical-
# izes the cross-verb cache key (the eventual fix) and returns hits, or
# it recompiles (status quo). Either way the (hits, misses) pair is
# populated and the gate has a number to compare.
find "${FIXTURE_DIR}" -name '*.rs' -exec touch {} +
find "${FIXTURE_DIR}" -name 'Cargo.toml' -exec touch {} +
find "${FIXTURE_DIR}" -name 'Cargo.lock' -exec touch {} +

# --- Cross-verb check pass -----------------------------------------
# Source mtimes advanced; content identical. cargo refingerprints and
# re-invokes rustc with `--emit=metadata` per unit. Each hop reaches
# zccache; this measures whether the cross-verb cache key is canonical-
# ized (the eventual fix) or split (status quo).

warm_start_ms="$(measure::now_ms)"
(
    cd "${FIXTURE_DIR}"
    SOLDR_CACHE_DIR="${CACHE}" soldr cargo check --release
)
warm_elapsed_ms="$(measure::elapsed_ms "${warm_start_ms}")"

measure::write_cache_report "${CACHE}" "${WORKDIR}/warm-cache-report.json"
measure::copy_zccache_logs_from_report \
    "${WORKDIR}/warm-cache-report.json" \
    "${WORKDIR}/warm-zccache-logs"
warm_hits="$(measure::cache_report_stat "${WORKDIR}/warm-cache-report.json" hits)"
warm_misses="$(measure::cache_report_stat "${WORKDIR}/warm-cache-report.json" misses)"
warm_hit_rate="$(measure::cache_report_stat "${WORKDIR}/warm-cache-report.json" hit_rate)"

SOLDR_CACHE_DIR="${CACHE}" soldr cache shutdown \
    --shutdown-timeout-seconds 5 --json >"${WORKDIR}/build-then-check-shutdown.json" || true

cache_bytes="$(measure::cache_bytes "${CACHE}")"

# --- Measurement teardown ------------------------------------------

measure::stop_rss_poller
trap - EXIT

peak_daemon_rss="$(measure::peak_daemon_rss_bytes "${RSS_CSV}")"
peak_compile_rss="$(measure::peak_compile_rss_bytes "${RSS_CSV}")"

# Speedup = cold (build) / warm (check). Today this is ~3x (close to
# the cold-cache hard-gate threshold) because check forces a full
# rebuild. Theoretical when keys canonicalize: 20x+ (check is just
# fingerprint walk + 100% cache lookups).
if (( warm_elapsed_ms > 0 )); then
    speedup="$(awk -v c="${cold_elapsed_ms}" -v w="${warm_elapsed_ms}" 'BEGIN { printf "%.2f", c / w }')"
else
    speedup="0.00"
fi

measure::emit_summary_json "${SCENARIO}" \
    "cold_ms=${cold_elapsed_ms}" \
    "warm_ms=${warm_elapsed_ms}" \
    "speedup=${speedup}" \
    "warm_hits=${warm_hits}" \
    "warm_misses=${warm_misses}" \
    "warm_hit_rate=${warm_hit_rate}" \
    "cache_bytes=${cache_bytes}" \
    "peak_daemon_rss_bytes=${peak_daemon_rss}" \
    "peak_compile_rss_bytes=${peak_compile_rss}"

measure::append_summary_md "| ${SCENARIO} | ${cold_elapsed_ms} ms | ${warm_elapsed_ms} ms | ${speedup}x | ${warm_hits}/${warm_misses} | ${warm_hit_rate} | $(( peak_daemon_rss / 1024 / 1024 )) MiB |"
