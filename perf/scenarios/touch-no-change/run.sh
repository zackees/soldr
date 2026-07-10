#!/usr/bin/env bash
# Scenario: cold build, advance every source-file mtime (simulating
# a tarball restore from a CI cache where mtimes are fresh but content
# is unchanged), rebuild. zccache's content-hash fingerprint must
# defeat cargo's mtime-based freshness so every unit hits.
#
# Pinned by issue #377 (soldr save/load — content-verified mtimes).
#
# Usage: run.sh <fixture-workdir>
set -euo pipefail

if (( $# != 1 )); then
    echo "usage: run.sh <fixture-workdir>" >&2
    exit 2
fi

FIXTURE_DIR="$(cd -- "$1" && pwd)"
SCENARIO="touch-no-change"

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../lib/common.sh
. "${HERE}/../../lib/common.sh"

WORKDIR="$(cd -- "${FIXTURE_DIR}/.." && pwd)"
CACHE="${WORKDIR}/cache-touch"
RSS_CSV="${WORKDIR}/rss-${SCENARIO}.csv"
REPETITIONS="${PERF_REPETITIONS:-3}"

mkdir -p "${CACHE}"
measure::prefetch_locked "${FIXTURE_DIR}"
root_package_id="$(cd "${FIXTURE_DIR}" && soldr cargo metadata \
    --locked --offline --no-deps --format-version=1 | \
    jq -r --arg manifest "${FIXTURE_DIR}/Cargo.toml" \
        '.packages[] | select(.manifest_path == $manifest) | .id')"

measure::start_rss_poller "${RSS_CSV}"
trap 'measure::stop_rss_poller' EXIT

# --- Cold build ----------------------------------------------------

cold_start_ms="$(measure::now_ms)"
(
    cd "${FIXTURE_DIR}"
    SOLDR_CACHE_DIR="${CACHE}" soldr cargo build --release \
        --locked --offline --message-format=json \
        >"${WORKDIR}/cargo-cold.jsonl"
)
cold_elapsed_ms="$(measure::elapsed_ms "${cold_start_ms}")"

# No `soldr cache flush` between cold and warm — the daemon stays
# alive for the very next `soldr cargo build --release` in the same
# session, so its in-memory depgraph serves the hits directly.
# See soldr#1156 (and the equivalent #1154 fix for build-then-check).

# --- Touch every source file without changing content --------------

# One `find` walk with alternation, not three (soldr#1154).
find "${FIXTURE_DIR}" \
    -path '*/target' -prune -o \
    -path '*/.git' -prune -o \
    -type f \( -name '*.rs' -o -name 'Cargo.toml' -o -name 'Cargo.lock' \) \
    -exec touch {} +

# Preserve target/: the lane measures Cargo's natural freshness decision.
# --- Warm builds (only the touched first-party unit should be Dirty) ---

warm_samples=()
warm_fresh_units=0
warm_dirty_units=0
warm_compiler_invocations=0
for ((rep = 1; rep <= REPETITIONS; rep++)); do
    if (( rep > 1 )); then
        find "${FIXTURE_DIR}" \
            -path '*/target' -prune -o \
            -path '*/.git' -prune -o \
            -type f \( -name '*.rs' -o -name 'Cargo.toml' -o -name 'Cargo.lock' \) \
            -exec touch {} +
    fi
    warm_start_ms="$(measure::now_ms)"
    (
        cd "${FIXTURE_DIR}"
        SOLDR_CACHE_DIR="${CACHE}" soldr cargo build --release \
            --locked --offline --message-format=json \
            >"${WORKDIR}/cargo-warm-${rep}.jsonl"
    )
    warm_samples+=("$(measure::elapsed_ms "${warm_start_ms}")")
    units_json="$(python3 "${HERE}/../../lib/cargo_units.py" \
        "${WORKDIR}/cargo-warm-${rep}.jsonl" \
        --root-package-id "${root_package_id}" \
        --expect-first-party-dirty 1)"
    fresh="$(jq -r '.fresh_units' <<<"${units_json}")"
    dirty="$(jq -r '.dirty_units' <<<"${units_json}")"
    warm_fresh_units=$(( warm_fresh_units + fresh ))
    warm_dirty_units=$(( warm_dirty_units + dirty ))
    warm_compiler_invocations=$(( warm_compiler_invocations + dirty ))
done
read -r warm_elapsed_ms warm_mad_ms < <(measure::median_and_mad "${warm_samples[@]}")
warm_samples_ms="$(IFS=,; echo "${warm_samples[*]}")"

measure::write_cache_report "${CACHE}" "${WORKDIR}/warm-cache-report.json"
measure::copy_zccache_logs_from_report \
    "${WORKDIR}/warm-cache-report.json" \
    "${WORKDIR}/warm-zccache-logs"
warm_hits="$(measure::cache_report_stat "${WORKDIR}/warm-cache-report.json" hits)"
warm_misses="$(measure::cache_report_stat "${WORKDIR}/warm-cache-report.json" misses)"
warm_hit_rate="$(measure::cache_report_stat "${WORKDIR}/warm-cache-report.json" hit_rate)"

SOLDR_CACHE_DIR="${CACHE}" soldr cache shutdown \
    --no-wait --json >"${WORKDIR}/touch-shutdown.json" || true

cache_bytes="$(measure::cache_bytes "${CACHE}")"

# --- Measurement teardown ------------------------------------------

measure::stop_rss_poller
trap - EXIT

peak_daemon_rss="$(measure::peak_daemon_rss_bytes "${RSS_CSV}")"
peak_compile_rss="$(measure::peak_compile_rss_bytes "${RSS_CSV}")"
peak_process_tree_rss="$(measure::peak_process_tree_rss_bytes "${RSS_CSV}")"

# Speedup = cold / warm (Nx). Guard against 0ms warm.
if (( warm_elapsed_ms > 0 )); then
    speedup="$(awk -v c="${cold_elapsed_ms}" -v w="${warm_elapsed_ms}" 'BEGIN { printf "%.2f", c / w }')"
else
    speedup="0.00"
fi

measure::emit_summary_json "${SCENARIO}" \
    "cold_ms=${cold_elapsed_ms}" \
    "warm_ms=${warm_elapsed_ms}" \
    "warm_mad_ms=${warm_mad_ms}" \
    "warm_samples_ms=${warm_samples_ms}" \
    "repetitions=${REPETITIONS}" \
    "warm_fresh_units=${warm_fresh_units}" \
    "warm_dirty_units=${warm_dirty_units}" \
    "warm_rustc_invocations=${warm_compiler_invocations}" \
    "speedup=${speedup}" \
    "warm_hits=${warm_hits}" \
    "warm_misses=${warm_misses}" \
    "warm_hit_rate=${warm_hit_rate}" \
    "cache_bytes=${cache_bytes}" \
    "cargo_locked=true" \
    "cargo_offline=true" \
    "fixture_reset=fresh-extraction" \
    "peak_daemon_rss_bytes=${peak_daemon_rss}" \
    "peak_compile_rss_bytes=${peak_compile_rss}" \
    "peak_process_tree_rss_bytes=${peak_process_tree_rss}"

measure::append_summary_md "| ${SCENARIO} | ${cold_elapsed_ms} ms | ${warm_elapsed_ms} ms | ${speedup}x | ${warm_hits}/${warm_misses} | ${warm_hit_rate} | $(( peak_daemon_rss / 1024 / 1024 )) MiB |"
