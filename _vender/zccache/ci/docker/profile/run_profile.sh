#!/usr/bin/env bash
# Entrypoint for the Linux profiler image. Builds the daemon + the
# perf_bench_test binary with debug symbols, then runs the chosen
# workload under simultaneous on-CPU (perf) and off-CPU (bpftrace)
# samplers, and emits two SVG flame charts.
#
# Outputs land in /out (host-side: .codex-artifacts/profile-linux-docker-...).

set -euo pipefail

OUT_DIR=${OUT_DIR:-/out}
# Default to the heavy compile bench — 50 cold + 50×5 warm rustc invocations
# (~135 s on Linux), which gives the on/off-CPU samplers enough events to
# resolve daemon hot paths. Override with ZCCACHE_PROFILE_WORKLOAD=…
WORKLOAD=${ZCCACHE_PROFILE_WORKLOAD:-perf_rustc_zccache_vs_sccache}
SAMPLE_HZ=${ZCCACHE_PROFILE_HZ:-99}

mkdir -p "${OUT_DIR}"

log() { echo "[$(date -u +%H:%M:%S)] $*" | tee -a "${OUT_DIR}/workload.log"; }

log "=== zccache Linux profiler ==="
log "workload     : ${WORKLOAD}"
log "sample rate  : ${SAMPLE_HZ} Hz"
log "out dir      : ${OUT_DIR}"
log "kernel       : $(uname -r)"
log "perf version : $(perf --version 2>&1 || true)"
log "bpftrace ver : $(bpftrace --version 2>&1 | head -1 || true)"

# Loosen kernel perf restrictions inside the container. May fail silently
# if the kernel disallows write — we then fall back to perf record's
# user-space-only mode further below.
sysctl -w kernel.perf_event_paranoid=-1 2>/dev/null || \
    log "warn: cannot lower perf_event_paranoid (proceeding with current value)"
sysctl -w kernel.kptr_restrict=0 2>/dev/null || true

# Build the bench binary with frame pointers so perf gets clean stacks.
# RUSTFLAGS preserves frame pointers; CARGO_PROFILE_DEV_DEBUG=2 keeps
# DWARF for fallback dwarf unwinding when fp unwinding misses.
cd /work
export CARGO_HOME=/tmp/cargo-home
export RUSTUP_HOME=/tmp/rustup-home
export CARGO_TARGET_DIR=/tmp/zccache-target
export RUSTFLAGS="-C force-frame-pointers=yes -C debuginfo=2"

log "==> compiling perf_bench_test (debug + frame-pointers) ..."
# Use --no-run so we only get the test executable; we'll invoke it ourselves.
# Pin worker threads low so the on-CPU profile isn't drowned by build noise.
cargo test --no-run -p zccache --test perf_bench_test 2>&1 \
    | tee -a "${OUT_DIR}/workload.log" | tail -5
TEST_BIN=$(ls -t /tmp/zccache-target/debug/deps/perf_bench_test-* \
    | grep -v '\.d$' | grep -v '\.json$' | head -1)
log "test binary  : ${TEST_BIN}"

# Locate rustc on PATH for the bench (it auto-discovers).
RUSTC=$(command -v rustc)
log "rustc        : ${RUSTC} ($("${RUSTC}" --version))"

# Find an available archiver (the C-archive bench tolerates ar.exe missing).
log "archiver     : $(command -v ar 2>&1 || echo 'not found')"

# Off-CPU sampler must be running BEFORE the workload spawns so the first
# context-switch samples are captured. Same for the on-CPU sampler. We
# wrap the workload in `perf record --` for the on-CPU pass so attach is
# atomic with launch, and run the off-CPU samplers system-wide in
# parallel.
log "==> launching workload (under perf record on-CPU sampler)"

# Off-CPU sampler (bpftrace) — runs in background for full duration. Uses
# the tracepoint:sched_switch recipe that delta-encodes off-CPU time per
# (ustack, kstack, comm). bpftrace's stack-id lookups fail intermittently
# on the WSL2 kernel; perf-sched is the primary off-CPU source below.
OFFCPU_BT="/tmp/offcpu.bt"
log "==> starting off-CPU sampler (bpftrace, best-effort)"
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

# Off-CPU primary: perf record sched_switch events with frame-pointer
# call-graphs. Runs the workload as its child so the data file is
# finalized cleanly when the workload exits (no SIGINT/SIGKILL races).
log "==> starting off-CPU sampler (perf sched record, system-wide)"
perf record -e sched:sched_switch \
    -g --call-graph fp \
    -a \
    -o "/tmp/perf-sched.data" \
    -- sleep 300 > /dev/null 2> "${OUT_DIR}/perf-sched.log" &
PERF_SCHED_PID=$!
log "perf-sched pid : ${PERF_SCHED_PID}"

# Give samplers a beat to attach before the workload runs.
sleep 2

# Run the workload UNDER perf record (on-CPU). The `--` form guarantees
# perf waits for the child to exit and finalizes the data file (no
# truncated header issue we hit with -p + SIGINT).
log "==> starting on-CPU sampler (perf record -- workload)"
perf record -F "${SAMPLE_HZ}" -g --call-graph fp \
    -o "/tmp/perf.data" \
    -- "${TEST_BIN}" "${WORKLOAD}" --nocapture --ignored --test-threads=1 \
    > "${OUT_DIR}/workload.stdout.log" 2> "${OUT_DIR}/workload.stderr.log"
PERF_EXIT=$?
log "on-CPU perf exit code: ${PERF_EXIT}"

# Stop the off-CPU samplers cleanly (they're still in the 300s sleep).
log "==> stopping off-CPU samplers"
kill -INT "${PERF_SCHED_PID:-0}" 2>/dev/null || true
kill -INT "${BPF_PID:-0}" 2>/dev/null || true
sleep 3
kill -KILL "${PERF_SCHED_PID:-0}" 2>/dev/null || true
kill -KILL "${BPF_PID:-0}" 2>/dev/null || true
# Give perf-sched a final beat to finalize its data file.
sleep 2

# Move perf data files to /out so they survive the container.
if [[ -s "/tmp/perf.data" ]]; then
    cp "/tmp/perf.data" "${OUT_DIR}/perf.data"
fi
if [[ -s "/tmp/perf-sched.data" ]]; then
    cp "/tmp/perf-sched.data" "${OUT_DIR}/perf-sched.data"
fi

# Render on-CPU flame chart.
log "==> rendering on-CPU flame chart"
if [[ -s "/tmp/perf.data" ]]; then
    perf script -i "/tmp/perf.data" > "/tmp/perf.script" \
        2>> "${OUT_DIR}/perf.log" || log "warn: perf script failed"
    stackcollapse-perf.pl "/tmp/perf.script" > "${OUT_DIR}/oncpu.folded" \
        2>> "${OUT_DIR}/perf.log" || log "warn: collapse failed"
    flamegraph.pl --title "zccache on-CPU (${WORKLOAD})" \
        --subtitle "Linux Docker $(uname -r) / ${SAMPLE_HZ} Hz" \
        "${OUT_DIR}/oncpu.folded" > "${OUT_DIR}/oncpu.svg" \
        2>> "${OUT_DIR}/perf.log" || log "warn: flamegraph failed"
    log "on-CPU folded entries: $(wc -l < "${OUT_DIR}/oncpu.folded" 2>/dev/null || echo 0)"
else
    log "warn: no perf.data captured — skipping on-CPU chart"
fi

# Render off-CPU flame chart. Prefer bpftrace output if it has data;
# otherwise fall back to perf sched events which are always parseable.
log "==> rendering off-CPU flame chart"
RENDERED_OFFCPU=0
if [[ -s "${OFFCPU_BT}" ]] && grep -q '@offcpu_us' "${OFFCPU_BT}"; then
    stackcollapse-bpftrace.pl "${OFFCPU_BT}" > "${OUT_DIR}/offcpu.folded" \
        2>> "${OUT_DIR}/offcpu.bpftrace.log" || log "warn: offcpu collapse failed"
    if [[ -s "${OUT_DIR}/offcpu.folded" ]]; then
        flamegraph.pl --color=io --countname=us --title "zccache off-CPU (${WORKLOAD})" \
            --subtitle "blocked-stack µs (bpftrace) — Linux Docker $(uname -r)" \
            "${OUT_DIR}/offcpu.folded" > "${OUT_DIR}/offcpu.svg" \
            2>> "${OUT_DIR}/offcpu.bpftrace.log" && RENDERED_OFFCPU=1
    fi
fi
if [[ "${RENDERED_OFFCPU}" -eq 0 && -s "/tmp/perf-sched.data" ]]; then
    log "==> bpftrace had no data — rendering perf-sched off-CPU fallback"
    perf script -i "/tmp/perf-sched.data" --no-inline > "/tmp/perf-sched.script" \
        2>> "${OUT_DIR}/perf-sched.log" || log "warn: perf-sched script failed"
    stackcollapse-perf.pl --kernel "/tmp/perf-sched.script" \
        > "${OUT_DIR}/offcpu.folded" \
        2>> "${OUT_DIR}/perf-sched.log" || log "warn: perf-sched collapse failed"
    if [[ -s "${OUT_DIR}/offcpu.folded" ]]; then
        flamegraph.pl --color=io --countname=switches --title "zccache off-CPU (${WORKLOAD})" \
            --subtitle "sched_switch stacks (perf sched fallback) — Linux Docker $(uname -r)" \
            "${OUT_DIR}/offcpu.folded" > "${OUT_DIR}/offcpu.svg" \
            2>> "${OUT_DIR}/perf-sched.log" && RENDERED_OFFCPU=1
    fi
fi
if [[ "${RENDERED_OFFCPU}" -eq 1 ]]; then
    log "off-CPU folded entries: $(wc -l < "${OUT_DIR}/offcpu.folded" 2>/dev/null || echo 0)"
else
    log "warn: no off-CPU chart could be rendered"
fi

log "==> done"
ls -la "${OUT_DIR}" | tee -a "${OUT_DIR}/workload.log"
