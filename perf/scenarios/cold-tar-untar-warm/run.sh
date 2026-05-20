#!/usr/bin/env bash
# Scenario: cold build, snapshot the cache via tar, untar into a
# fresh cache root, warm build against the restored state.
#
# Isolates pure archive fidelity from any GitHub Actions cache layer:
# if this row is green and `cold-teardown-warm` (a future cross-job
# scenario) is red, the problem is GHA's cache key/scope, not soldr.
#
# Usage: run.sh <fixture-workdir>
set -euo pipefail

if (( $# != 1 )); then
    echo "usage: run.sh <fixture-workdir>" >&2
    exit 2
fi

FIXTURE_DIR="$1"
SCENARIO="cold-tar-untar-warm"

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../lib/common.sh
. "${HERE}/../../lib/common.sh"

WORKDIR="$(cd -- "${FIXTURE_DIR}/.." && pwd)"
CACHE_COLD="${WORKDIR}/cache-cold"
CACHE_WARM="${WORKDIR}/cache-warm"
SNAPSHOT="${WORKDIR}/cache-snapshot.tar.gz"
RSS_CSV="${WORKDIR}/rss-${SCENARIO}.csv"

mkdir -p "${CACHE_COLD}" "${CACHE_WARM}"

measure::start_rss_poller "${RSS_CSV}"
trap 'measure::stop_rss_poller' EXIT

# --- Cold build ----------------------------------------------------

cold_start_ms="$(measure::now_ms)"
(
    cd "${FIXTURE_DIR}"
    SOLDR_CACHE_DIR="${CACHE_COLD}" soldr cargo build --release
)
cold_elapsed_ms="$(measure::elapsed_ms "${cold_start_ms}")"

# Flush + shutdown so the depgraph snapshot is durable before tar.
SOLDR_CACHE_DIR="${CACHE_COLD}" soldr cache flush --json >/dev/null 2>&1 || true
SOLDR_CACHE_DIR="${CACHE_COLD}" soldr cache shutdown \
    --shutdown-timeout-seconds 30 --json >"${WORKDIR}/cold-shutdown.json" || true

cold_cache_bytes="$(measure::cache_bytes "${CACHE_COLD}")"

# --- Snapshot ------------------------------------------------------

tar -C "${CACHE_COLD}" -czf "${SNAPSHOT}" cache
tar_bytes="$(wc -c <"${SNAPSHOT}")"

# --- Restore into a clean cache dir --------------------------------

tar -C "${CACHE_WARM}" -xzf "${SNAPSHOT}"

# Force cargo to think every unit needs to be recompiled. soldr will
# then ask zccache for each unit; hit rate measures restore fidelity.
(cd "${FIXTURE_DIR}" && cargo clean >/dev/null 2>&1)

# --- Warm build ----------------------------------------------------

warm_start_ms="$(measure::now_ms)"
(
    cd "${FIXTURE_DIR}"
    SOLDR_CACHE_DIR="${CACHE_WARM}" soldr cargo build --release
)
warm_elapsed_ms="$(measure::elapsed_ms "${warm_start_ms}")"

warm_stats="$(SOLDR_CACHE_DIR="${CACHE_WARM}" measure::session_end_json)"
warm_hits="$(echo "${warm_stats}" | jq -r '.stats.hits // 0')"
warm_misses="$(echo "${warm_stats}" | jq -r '.stats.misses // 0')"
warm_hit_rate="$(echo "${warm_stats}" | jq -r '.stats.hit_rate // 0')"

SOLDR_CACHE_DIR="${CACHE_WARM}" soldr cache shutdown \
    --shutdown-timeout-seconds 30 --json >"${WORKDIR}/warm-shutdown.json" || true

warm_cache_bytes="$(measure::cache_bytes "${CACHE_WARM}")"

# --- Measurement teardown ------------------------------------------

measure::stop_rss_poller
trap - EXIT

peak_daemon_rss="$(measure::peak_daemon_rss_bytes "${RSS_CSV}")"
peak_compile_rss="$(measure::peak_compile_rss_bytes "${RSS_CSV}")"

# Speedup = cold / warm (Nx). 0 warm_ms means a measurement bug, not a
# win — emit 0 instead of inf so the evaluate gate can flag it.
if (( warm_elapsed_ms > 0 )); then
    speedup="$(awk -v c="${cold_elapsed_ms}" -v w="${warm_elapsed_ms}" 'BEGIN { printf "%.2f", c / w }')"
else
    speedup="0.00"
fi

# --- Emit -----------------------------------------------------------

measure::emit_summary_json "${SCENARIO}" \
    "cold_ms=${cold_elapsed_ms}" \
    "warm_ms=${warm_elapsed_ms}" \
    "speedup=${speedup}" \
    "warm_hits=${warm_hits}" \
    "warm_misses=${warm_misses}" \
    "warm_hit_rate=${warm_hit_rate}" \
    "cold_cache_bytes=${cold_cache_bytes}" \
    "warm_cache_bytes=${warm_cache_bytes}" \
    "tarball_bytes=${tar_bytes}" \
    "peak_daemon_rss_bytes=${peak_daemon_rss}" \
    "peak_compile_rss_bytes=${peak_compile_rss}"

measure::append_summary_md "| ${SCENARIO} | ${cold_elapsed_ms} ms | ${warm_elapsed_ms} ms | ${speedup}x | ${warm_hits}/${warm_misses} | ${warm_hit_rate} | $(( peak_daemon_rss / 1024 / 1024 )) MiB |"
