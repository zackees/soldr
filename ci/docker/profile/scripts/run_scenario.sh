#!/usr/bin/env bash
# Run one perf-matrix scenario under simultaneous on-CPU (perf record)
# and off-CPU (bpftrace sched_switch, perf-sched fallback) samplers.
#
# Usage: run_scenario.sh <scenario-name> <fixture-dir> <out-dir>
#
# Extracted from the earlier cold/warm/bare loop in run_profile.sh so
# the same sampler wiring drives the four `perf/scenarios/*/run.sh`
# scripts. The scenario script itself is invoked as a subprocess and
# receives the fixture-dir as its single argument (matching the
# perf-matrix convention in `perf/README.md`).
#
# soldr#1244+ profiling harness: on-CPU + off-CPU only. Tokio-events
# profiling is a separate follow-up (would require console-subscriber
# wired into soldr-daemon; deferred).

set -euo pipefail

if (( $# != 3 )); then
    echo "usage: run_scenario.sh <scenario-name> <fixture-dir> <out-dir>" >&2
    exit 2
fi

SCENARIO_NAME="$1"
FIXTURE_DIR="$2"
OUT_DIR="$3"

SAMPLE_HZ=${SOLDR_PROFILE_HZ:-99}
SCENARIO_SCRIPT="/work/perf/scenarios/${SCENARIO_NAME}/run.sh"

if [[ ! -x "${SCENARIO_SCRIPT}" ]] && [[ ! -f "${SCENARIO_SCRIPT}" ]]; then
    echo "run_scenario.sh: scenario script not found: ${SCENARIO_SCRIPT}" >&2
    exit 3
fi

mkdir -p "${OUT_DIR}"

log() { echo "[$(date -u +%H:%M:%S)] [${SCENARIO_NAME}] $*" | tee -a "${OUT_DIR}/scenario.log"; }

log "==[scenario: ${SCENARIO_NAME}]=="
log "scenario script : ${SCENARIO_SCRIPT}"
log "fixture dir     : ${FIXTURE_DIR}"
log "out dir         : ${OUT_DIR}"
log "sample hz       : ${SAMPLE_HZ}"

# Off-CPU sampler #1 — bpftrace sched_switch. Sums blocked microseconds
# per (ustack, kstack, comm). WSL2 kernels can refuse the stack-id
# lookup; the perf-sched fallback below picks up in that case.
log "==> starting off-CPU sampler (bpftrace, best-effort)"
OFFCPU_BT="/tmp/offcpu-${SCENARIO_NAME}.bt"
bpftrace -B none -o "${OFFCPU_BT}" -e '
tracepoint:sched:sched_switch
{
    @start[args->prev_pid] = nsecs;
    if (@start[args->next_pid] != 0) {
        $delta_us = (nsecs - @start[args->next_pid]) / 1000;
        @offcpu_us[ustack, kstack, comm] = sum($delta_us);
        delete(@start[args->next_pid]);
    }
}

interval:s:600 { exit(); }
END { clear(@start); }
' > "${OUT_DIR}/offcpu.bpftrace.log" 2>&1 &
BPF_PID=$!
log "bpftrace pid : ${BPF_PID}"

# Off-CPU sampler #2 — perf-sched fallback. Runs concurrently so we
# always have something to fold when bpftrace's stack-id lookup fails.
log "==> starting off-CPU sampler (perf sched record, system-wide)"
perf record -e sched:sched_switch \
    -g --call-graph fp \
    -a \
    -o "/tmp/perf-sched-${SCENARIO_NAME}.data" \
    -- sleep 300 > /dev/null 2> "${OUT_DIR}/perf-sched.log" &
PERF_SCHED_PID=$!
log "perf-sched pid : ${PERF_SCHED_PID}"

# Let both samplers latch before the workload starts.
sleep 2

# On-CPU sampler — perf record wrapping the scenario invocation.
log "==> starting on-CPU sampler wrapping ${SCENARIO_SCRIPT}"
perf record -F "${SAMPLE_HZ}" -g --call-graph fp \
    -o "/tmp/perf-${SCENARIO_NAME}.data" \
    -- bash "${SCENARIO_SCRIPT}" "${FIXTURE_DIR}" \
    > "${OUT_DIR}/scenario.stdout.log" 2> "${OUT_DIR}/scenario.stderr.log" || true
PERF_EXIT=$?
log "on-CPU perf exit code: ${PERF_EXIT}"

log "==> stopping off-CPU samplers"
kill -INT "${PERF_SCHED_PID}" 2>/dev/null || true
kill -INT "${BPF_PID}" 2>/dev/null || true
sleep 3
kill -KILL "${PERF_SCHED_PID}" 2>/dev/null || true
kill -KILL "${BPF_PID}" 2>/dev/null || true
sleep 2

# Move raw perf data into the scenario out dir.
[[ -s "/tmp/perf-${SCENARIO_NAME}.data" ]] && cp "/tmp/perf-${SCENARIO_NAME}.data" "${OUT_DIR}/perf.data"
[[ -s "/tmp/perf-sched-${SCENARIO_NAME}.data" ]] && cp "/tmp/perf-sched-${SCENARIO_NAME}.data" "${OUT_DIR}/perf-sched.data"

# --- on-CPU flame chart --------------------------------------------
log "==> rendering on-CPU flame chart"
if [[ -s "${OUT_DIR}/perf.data" ]]; then
    perf script -i "${OUT_DIR}/perf.data" > "/tmp/perf-${SCENARIO_NAME}.script" \
        2>> "${OUT_DIR}/perf.log" || log "warn: perf script failed"
    stackcollapse-perf.pl "/tmp/perf-${SCENARIO_NAME}.script" > "${OUT_DIR}/oncpu.folded" \
        2>> "${OUT_DIR}/perf.log" || log "warn: collapse failed"
    flamegraph.pl --title "soldr on-CPU (${SCENARIO_NAME})" \
        --subtitle "Linux Docker $(uname -r) / ${SAMPLE_HZ} Hz / perf-matrix scenario" \
        "${OUT_DIR}/oncpu.folded" > "${OUT_DIR}/oncpu.svg" \
        2>> "${OUT_DIR}/perf.log" || log "warn: flamegraph failed"
    log "on-CPU folded entries: $(wc -l < "${OUT_DIR}/oncpu.folded" 2>/dev/null || echo 0)"
else
    log "warn: no perf.data captured — skipping on-CPU chart"
fi

# --- off-CPU flame chart -------------------------------------------
log "==> rendering off-CPU flame chart"
RENDERED_OFFCPU=0
if [[ -s "${OFFCPU_BT}" ]] && grep -q '@offcpu_us' "${OFFCPU_BT}"; then
    stackcollapse-bpftrace.pl "${OFFCPU_BT}" > "${OUT_DIR}/offcpu.folded" \
        2>> "${OUT_DIR}/offcpu.bpftrace.log" || log "warn: offcpu collapse failed"
    if [[ -s "${OUT_DIR}/offcpu.folded" ]]; then
        flamegraph.pl --color=io --countname=us --title "soldr off-CPU (${SCENARIO_NAME})" \
            --subtitle "blocked-stack µs (bpftrace) — Linux Docker $(uname -r)" \
            "${OUT_DIR}/offcpu.folded" > "${OUT_DIR}/offcpu.svg" \
            2>> "${OUT_DIR}/offcpu.bpftrace.log" && RENDERED_OFFCPU=1
    fi
fi
if [[ "${RENDERED_OFFCPU}" -eq 0 && -s "${OUT_DIR}/perf-sched.data" ]]; then
    log "==> bpftrace had no data — rendering perf-sched off-CPU fallback"
    perf script -i "${OUT_DIR}/perf-sched.data" --no-inline > "/tmp/perf-sched-${SCENARIO_NAME}.script" \
        2>> "${OUT_DIR}/perf-sched.log" || log "warn: perf-sched script failed"
    stackcollapse-perf.pl --kernel "/tmp/perf-sched-${SCENARIO_NAME}.script" \
        > "${OUT_DIR}/offcpu.folded" \
        2>> "${OUT_DIR}/perf-sched.log" || log "warn: perf-sched collapse failed"
    if [[ -s "${OUT_DIR}/offcpu.folded" ]]; then
        flamegraph.pl --color=io --countname=switches --title "soldr off-CPU (${SCENARIO_NAME})" \
            --subtitle "sched_switch stacks (perf sched fallback) — Linux Docker $(uname -r)" \
            "${OUT_DIR}/offcpu.folded" > "${OUT_DIR}/offcpu.svg" \
            2>> "${OUT_DIR}/perf-sched.log" && RENDERED_OFFCPU=1
    fi
fi
if [[ "${RENDERED_OFFCPU}" -eq 1 ]]; then
    log "off-CPU folded entries: $(wc -l < "${OUT_DIR}/offcpu.folded" 2>/dev/null || echo 0)"
else
    log "warn: no off-CPU chart could be rendered for ${SCENARIO_NAME}"
fi

log "==[done: ${SCENARIO_NAME}]=="
