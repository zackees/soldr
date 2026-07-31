# shellcheck shell=bash
# Common helpers for perf matrix workers. Source this file with
# `. "${LIB_DIR}/common.sh"` from a scenario script.
#
# Conventions
# -----------
# * Every function lives under a `measure::` namespace.
# * State (timestamps, PIDs, CSV paths) is kept in process-local
#   globals named `_MEASURE_*` so two callers in the same shell can
#   round-trip cleanly. Scenarios that fan out should source this
#   file in each subshell rather than share state.
# * Output for `$GITHUB_STEP_SUMMARY` is markdown; output for the
#   master aggregator is JSON on stdout.

# --- RSS sidecar ---------------------------------------------------

# measure::start_rss_poller <csv-path>
#
# Backgrounds a 1Hz `ps` loop that appends `epoch,pid,rss,vsz,comm`
# rows for every running zccache-daemon / rustc / cargo process. The
# poller PID is stashed so `measure::stop_rss_poller` can kill it.
measure::start_rss_poller() {
    local csv="$1"
    _MEASURE_RSS_CSV="${csv}"
    echo "epoch,pid,rss_kb,vsz_kb,comm" > "${csv}"
    (
        while true; do
            # `ps -A -o pid,rss,vsz,comm --no-headers` is GNU-flavoured;
            # ubuntu runners have it. macOS workers (v2+) will need a
            # different code path.
            local now
            now="$(date +%s)"
            ps -A -o pid=,rss=,vsz=,comm= 2>/dev/null \
                | awk -v t="${now}" '
                    $4 == "soldr-daemon" {
                        printf "%s,%s,%s,%s,%s\n", t, $1, $2, $3, $4
                    }
                    $4 ~ /^(zccache-daemon|zccache|rustc|cargo|soldr)$/ {
                        printf "%s,%s,%s,%s,%s\n", t, $1, $2, $3, $4
                    }' \
                >> "${csv}" || true
            sleep 1
        done
    ) &
    _MEASURE_RSS_PID="$!"
    # Detach so the poller survives `set -e` traps in the parent.
    disown "${_MEASURE_RSS_PID}" 2>/dev/null || true
}

# measure::stop_rss_poller
#
# Kills the background poller started by `start_rss_poller`. Safe to
# call when no poller is running.
measure::stop_rss_poller() {
    if [[ -n "${_MEASURE_RSS_PID:-}" ]]; then
        kill "${_MEASURE_RSS_PID}" 2>/dev/null || true
        wait "${_MEASURE_RSS_PID}" 2>/dev/null || true
        _MEASURE_RSS_PID=""
    fi
}

# measure::peak_daemon_rss_bytes <csv-path>
#
# Prints the largest zccache-daemon RSS observed in the CSV (in
# bytes). Prints `0` if no daemon rows are present.
measure::peak_daemon_rss_bytes() {
    local csv="$1"
    awk -F, '
        NR == 1 { next }
        $5 == "soldr-daemon" || $5 == "zccache-daemon" || $5 == "zccache" {
            by_epoch[$1] += $3 + 0
        }
        END {
            for (epoch in by_epoch) if (by_epoch[epoch] > peak) peak = by_epoch[epoch]
            print (peak ? peak : 0) * 1024
        }
    ' "${csv}"
}

# Peak aggregate RSS for every matching process at a sample instant. CI jobs
# are isolated; shared local hosts can contaminate this diagnostic.
measure::peak_process_tree_rss_bytes() {
    local csv="$1"
    awk -F, '
        NR == 1 { next }
        { by_epoch[$1] += $3 + 0 }
        END {
            for (epoch in by_epoch) if (by_epoch[epoch] > peak) peak = by_epoch[epoch]
            print (peak ? peak : 0) * 1024
        }
    ' "${csv}"
}

# measure::peak_compile_rss_bytes <csv-path>
#
# Peak rustc + cargo RSS seen across the whole CSV.
measure::peak_compile_rss_bytes() {
    local csv="$1"
    awk -F, '
        NR == 1 { next }
        $5 == "rustc" || $5 == "cargo" {
            kb = $3 + 0
            if (kb > peak) peak = kb
        }
        END { print (peak ? peak : 0) * 1024 }
    ' "${csv}"
}

# --- Disk footprint -------------------------------------------------

# measure::cache_bytes <cache-root>
#
# Total bytes under <cache-root>/cache/zccache. The standard soldr
# layout puts everything cache-related there; the scenario points
# $SOLDR_CACHE_DIR at the parent so the same path resolves on disk.
measure::cache_bytes() {
    local cache_root="$1"
    local zccache_dir="${cache_root}/cache/zccache"
    if [[ -d "${zccache_dir}" ]]; then
        # soldr#1942: the daemon writes metadata atomically -- create
        # `.metadata.bin.tmp-<pid>`, then rename it into place. When that
        # rename lands between du's readdir and its stat, du reports the
        # vanished path and exits non-zero, and under `pipefail` that fails
        # the whole scenario *after* a successful build.
        #
        # The file is supposed to disappear and its size is noise against a
        # cache-size metric, so tolerate it: du's total over what it did see
        # is the answer we want. `2>/dev/null` alone would not fix this --
        # the failure is the exit status crossing the pipe, not the message.
        local bytes
        bytes="$(du -sb "${zccache_dir}" 2>/dev/null | awk '{print $1}')" || true
        echo "${bytes:-0}"
    else
        echo 0
    fi
}

# --- Soldr stats wrappers -------------------------------------------

# measure::session_end_json <session-id-or-empty>
#
# Run `soldr session-end --json` and print the parsed JSON on stdout.
# When no session id is given soldr uses $ZCCACHE_SESSION_ID.
# Returns an empty object if the call fails (the scenario is still
# useful when, e.g., the daemon never started a session).
measure::session_end_json() {
    local id="${1:-}"
    local cmd=("soldr" "session-end" "--json")
    if [[ -n "${id}" ]]; then
        cmd+=("--id" "${id}")
    fi
    if out="$("${cmd[@]}" 2>/dev/null)"; then
        echo "${out}"
    else
        echo "{}"
    fi
}

# measure::write_cache_report <cache-root> <json-path>
#
# Persist `soldr cache report --json` for <cache-root>. On failure, write
# an empty object so callers still emit a result.json and the evaluator can
# fail with BAD-STATS instead of losing the scenario output.
measure::write_cache_report() {
    local cache_root="$1"
    local out="$2"
    if ! SOLDR_CACHE_DIR="${cache_root}" soldr cache report --json > "${out}" 2>/dev/null; then
        printf '{}\n' > "${out}"
    fi
}

# measure::cache_report_stat <json-path> <stat-key>
#
# Read a hit/miss/rate field from `soldr cache report --json`. Supports both
# direct zccache stats and older nested `{ "stats": ... }` shapes.
measure::cache_report_stat() {
    local report="$1"
    local key="$2"
    jq -r --arg k "${key}" \
        '.last_session.stats[$k] // .last_session[$k] // 0' \
        "${report}" 2>/dev/null || echo 0
}

# measure::copy_zccache_logs_from_report <json-path> <dest-dir>
#
# Copy the authoritative zccache logs directory referenced by a cache report.
# This follows private-daemon paths such as
# cache/zccache/private/<daemon>/logs instead of assuming cache/zccache/logs.
measure::copy_zccache_logs_from_report() {
    local report="$1"
    local dest="$2"
    local stats_path
    stats_path="$(jq -r '.session_stats_path // empty' "${report}" 2>/dev/null || true)"
    if [[ -z "${stats_path}" ]]; then
        return 0
    fi
    local logs_dir
    logs_dir="$(dirname -- "${stats_path}")"
    if [[ -d "${logs_dir}" ]]; then
        rm -rf "${dest}"
        cp -R "${logs_dir}" "${dest}"
    fi
}

# --- Wall-time --------------------------------------------------------

# measure::now_ms
measure::now_ms() {
    if [[ -r /proc/uptime ]]; then
        awk '{ printf "%d\n", $1 * 1000 }' /proc/uptime
    else
        python3 -c 'import time; print(time.monotonic_ns() // 1000000)'
    fi
}

# measure::elapsed_ms <start-ms>
measure::elapsed_ms() {
    local start="$1"
    local now
    now="$(measure::now_ms)"
    echo $(( now - start ))
}

# Prints `<median> <median-absolute-deviation>` for integer samples.
measure::median_and_mad() {
    local -a sorted deviations
    # bash 3.2 has no `mapfile`; perf-matrix.yml notes the mac rows land
    # here once this file is cross-platform, so it must not need bash 4.
    sorted=()
    while IFS= read -r sorted_value; do
        sorted+=("${sorted_value}")
    done < <(printf '%s
' "$@" | sort -n)
    local count="${#sorted[@]}"
    if (( count == 0 )); then
        echo "0 0"
        return
    fi
    local median="${sorted[$((count / 2))]}" value delta
    for value in "${sorted[@]}"; do
        delta=$(( value - median ))
        (( delta < 0 )) && delta=$(( -delta ))
        deviations+=("${delta}")
    done
    # Sorted into a NEW array deliberately: `deviations=()` before the loop
    # would run before the process substitution reads it, sorting nothing.
    local -a sorted_deviations=()
    while IFS= read -r deviation_value; do
        sorted_deviations+=("${deviation_value}")
    done < <(printf '%s
' "${deviations[@]}" | sort -n)
    echo "${median} ${sorted_deviations[$((count / 2))]}"
}

# Acquire dependencies outside measured intervals. Timed commands are offline.
measure::prefetch_locked() {
    local fixture_dir="$1"
    (cd "${fixture_dir}" && soldr cargo metadata --locked \
        --format-version=1 >/dev/null)
}

# --- Summary emission -----------------------------------------------

# measure::emit_summary_json <scenario> <key=value>...
#
# Prints a single JSON object on stdout with the provided key/value
# pairs (all values are emitted as strings unless they match a
# number-only regex, in which case they are emitted as JSON numbers).
# A `scenario` key is always included.
measure::emit_summary_json() {
    local scenario="$1"; shift
    local first=1
    printf '{"scenario":"%s"' "${scenario}"
    for kv in "$@"; do
        local key="${kv%%=*}"
        local value="${kv#*=}"
        printf ','
        if [[ "${value}" =~ ^-?[0-9]+(\.[0-9]+)?$ ]]; then
            printf '"%s":%s' "${key}" "${value}"
        else
            # Naive JSON-string escape: backslash + double quote.
            local escaped="${value//\\/\\\\}"
            escaped="${escaped//\"/\\\"}"
            printf '"%s":"%s"' "${key}" "${escaped}"
        fi
        first=0
    done
    printf '}\n'
}

# measure::append_summary_md <table-row>
#
# Append a single markdown row to $GITHUB_STEP_SUMMARY when running
# inside a GHA worker. No-op locally so scripts stay testable.
measure::append_summary_md() {
    if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
        echo "$1" >> "${GITHUB_STEP_SUMMARY}"
    fi
}

# measure::reset_cache_dir <cache-root>
#
# Wipe a soldr cache root so the next build starts cold. Stops the
# daemon first so we do not race the file system.
measure::reset_cache_dir() {
    local cache_root="$1"
    if command -v soldr >/dev/null 2>&1; then
        SOLDR_CACHE_DIR="${cache_root}" soldr cache shutdown \
            --shutdown-timeout-seconds 15 --json >/dev/null 2>&1 || true
    fi
    rm -rf "${cache_root}/cache" "${cache_root}/bin" 2>/dev/null || true
    mkdir -p "${cache_root}"
}
