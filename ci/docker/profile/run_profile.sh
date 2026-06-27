#!/usr/bin/env bash
# Entrypoint for the soldr Linux profiler image.
#
# Adapts zccache's ci/docker/profile/run_profile.sh to a soldr-wide
# workload: builds soldr-cli with debug symbols + frame pointers, then
# runs three scenarios (`cold` / `warm` / `bare`) of `soldr cargo build`
# against an extracted fixture under simultaneous on-CPU (perf) and
# off-CPU (bpftrace + perf-sched) samplers. Each scenario gets its own
# subdir under /out.
#
# Outputs land in /out (host-side: .codex-artifacts/soldr-wider-perf-...).

set -euo pipefail

OUT_DIR=${OUT_DIR:-/out}
SAMPLE_HZ=${SOLDR_PROFILE_HZ:-99}
SCENARIOS=${SOLDR_PROFILE_SCENARIOS:-cold warm bare}
WORKLOAD_FIXTURE=${SOLDR_PROFILE_FIXTURE:-/work/perf/fixtures/medium.tar.gz}
WORKLOAD_FIXTURE_DIR=${SOLDR_PROFILE_FIXTURE_DIR:-medium}

mkdir -p "${OUT_DIR}"

log() { echo "[$(date -u +%H:%M:%S)] $*" | tee -a "${OUT_DIR}/run.log"; }

log "=== soldr Linux profiler ==="
log "sample rate  : ${SAMPLE_HZ} Hz"
log "scenarios    : ${SCENARIOS}"
log "fixture      : ${WORKLOAD_FIXTURE}"
log "out dir      : ${OUT_DIR}"
log "kernel       : $(uname -r)"
log "perf version : $(perf --version 2>&1 || true)"
log "bpftrace ver : $(bpftrace --version 2>&1 | head -1 || true)"

# Loosen kernel perf restrictions inside the container.
sysctl -w kernel.perf_event_paranoid=-1 2>/dev/null || \
    log "warn: cannot lower perf_event_paranoid (proceeding with current value)"
sysctl -w kernel.kptr_restrict=0 2>/dev/null || true

cd /work

# Frame pointers + DWARF for clean perf stacks. Build soldr-cli once;
# every scenario reuses the same binary.
export CARGO_HOME=/tmp/cargo-home
export RUSTUP_HOME=/tmp/rustup-home
export CARGO_TARGET_DIR=/tmp/soldr-target
export RUSTFLAGS="-C force-frame-pointers=yes -C debuginfo=2"

# Rust 1.94.1 base image bundles rustup but does not have a default
# toolchain configured. Soldr's rust-toolchain.toml pins to 1.94.1; we
# install it explicitly so cargo proxies through cleanly. Idempotent.
log "==> installing rust toolchain 1.94.1 ..."
rustup toolchain install 1.94.1 --profile minimal --no-self-update 2>&1 \
    | tee -a "${OUT_DIR}/build.log" | tail -3
rustup default 1.94.1 2>&1 | tee -a "${OUT_DIR}/build.log" | tail -3

log "==> building soldr-cli + soldr-daemon (release + frame-pointers + debuginfo) ..."
# Build both binaries — the wrapper now auto-spawns soldr-daemon via
# `try_spawn_detached` when the IPC socket is missing, which requires
# the daemon binary to live alongside the soldr binary on PATH.
#
# Release mode matters: the embedded zccache compile service runs
# inside the soldr-daemon, so a debug-built daemon would underrun
# the baseline (which forked the release-built managed zccache.exe)
# by 10-20× on the IPC hot path. Keep `-C force-frame-pointers` +
# `-C debuginfo=2` so perf still resolves clean stacks.
cargo build --release -p soldr-cli --bin soldr --bin soldr-daemon 2>&1 \
    | tee -a "${OUT_DIR}/build.log" | tail -5
SOLDR_BIN=/tmp/soldr-target/release/soldr
if [[ ! -x "${SOLDR_BIN}" ]]; then
    log "fatal: soldr binary missing at ${SOLDR_BIN}"
    exit 64
fi
log "soldr binary : ${SOLDR_BIN}"

# Ensure soldr is on PATH so its env-detection wins over any host soldr.
export PATH="/tmp/soldr-target/release:${PATH}"

# Resolve the cargo + rustc that will run inside the fixture.
log "rustc        : $(command -v rustc) ($(rustc --version))"
log "cargo        : $(command -v cargo) ($(cargo --version))"

# Each scenario gets its own out-dir + its own fixture extract so cold
# means cold every time.
extract_fixture() {
    local dest=$1
    rm -rf "${dest}"
    mkdir -p "${dest}"
    tar -xzf "${WORKLOAD_FIXTURE}" -C "${dest}"
    # Some fixtures unpack to a subdir matching WORKLOAD_FIXTURE_DIR;
    # if so, return that path. Otherwise stay at the extract root.
    if [[ -d "${dest}/${WORKLOAD_FIXTURE_DIR}" ]]; then
        echo "${dest}/${WORKLOAD_FIXTURE_DIR}"
    else
        echo "${dest}"
    fi
}

run_scenario() {
    local name=$1
    local scen_out="${OUT_DIR}/${name}"
    mkdir -p "${scen_out}"

    log "==[scenario: ${name}]=="

    # Per-scenario workspace + per-scenario soldr cache.
    # soldr#981: route the cache dir under /out so daemon-spawn.log
    # and other diagnostic artifacts survive the container exit.
    local workspace="/tmp/workspace-${name}"
    local soldr_cache="${scen_out}/soldr-cache"
    mkdir -p "${soldr_cache}"

    local fixture_path
    fixture_path=$(extract_fixture "${workspace}")
    log "scenario fixture path : ${fixture_path}"

    # Build the command per scenario. `cold` and `warm` go through
    # soldr; `bare` skips soldr entirely as the theoretical-max anchor.
    local cmd
    case "${name}" in
        cold|warm)
            cmd=("${SOLDR_BIN}" cargo build --manifest-path "${fixture_path}/Cargo.toml")
            export SOLDR_CACHE_DIR="${soldr_cache}"
            # soldr#981 diagnostic: tell the embedded daemon to emit
            # per-phase JSONL into the scenario output dir. Off when
            # the var is unset (zero hot-path cost). Soaked at the
            # whole scenario including warm so the comparison is
            # apples-to-apples.
            export SOLDR_DAEMON_TRACE="${scen_out}/daemon-trace.jsonl"
            ;;
        bare)
            cmd=(cargo build --manifest-path "${fixture_path}/Cargo.toml")
            unset SOLDR_CACHE_DIR
            unset SOLDR_DAEMON_TRACE
            ;;
        *)
            log "unknown scenario ${name} — skipping"
            return 0
            ;;
    esac

    # Warm priming: for `warm`, first run a build to populate the cache,
    # then capture profile data on the SECOND build. This is what makes
    # `warm` the realistic "everything cached" baseline.
    if [[ "${name}" == "warm" ]]; then
        log "==> priming the cache before sampling"
        "${cmd[@]}" > "${scen_out}/priming.stdout.log" 2> "${scen_out}/priming.stderr.log" \
            || log "warn: priming run exited non-zero (sampling will proceed anyway)"
        # Clear the build target between priming and sampling so the
        # sampling run is a pure cache-hit pass, not a no-op.
        log "==> clearing target between priming and sampling"
        rm -rf "${fixture_path}/target"
    fi

    # Off-CPU sampler — bpftrace + perf-sched.
    log "==> starting off-CPU sampler (bpftrace, best-effort)"
    local offcpu_bt="/tmp/offcpu-${name}.bt"
    bpftrace -B none -o "${offcpu_bt}" -e '
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
' > "${scen_out}/offcpu.bpftrace.log" 2>&1 &
    local bpf_pid=$!
    log "bpftrace pid : ${bpf_pid}"

    log "==> starting off-CPU sampler (perf sched record, system-wide)"
    perf record -e sched:sched_switch \
        -g --call-graph fp \
        -a \
        -o "/tmp/perf-sched-${name}.data" \
        -- sleep 300 > /dev/null 2> "${scen_out}/perf-sched.log" &
    local perf_sched_pid=$!
    log "perf-sched pid : ${perf_sched_pid}"

    sleep 2

    log "==> starting on-CPU sampler (perf record -- workload)"
    log "command : ${cmd[*]}"
    perf record -F "${SAMPLE_HZ}" -g --call-graph fp \
        -o "/tmp/perf-${name}.data" \
        -- "${cmd[@]}" \
        > "${scen_out}/workload.stdout.log" 2> "${scen_out}/workload.stderr.log" || true
    local perf_exit=$?
    log "on-CPU perf exit code: ${perf_exit}"

    log "==> stopping off-CPU samplers"
    kill -INT "${perf_sched_pid}" 2>/dev/null || true
    kill -INT "${bpf_pid}" 2>/dev/null || true
    sleep 3
    kill -KILL "${perf_sched_pid}" 2>/dev/null || true
    kill -KILL "${bpf_pid}" 2>/dev/null || true
    sleep 2

    # Move raw perf data into the scenario out dir.
    [[ -s "/tmp/perf-${name}.data" ]] && cp "/tmp/perf-${name}.data" "${scen_out}/perf.data"
    [[ -s "/tmp/perf-sched-${name}.data" ]] && cp "/tmp/perf-sched-${name}.data" "${scen_out}/perf-sched.data"

    log "==> rendering on-CPU flame chart"
    if [[ -s "${scen_out}/perf.data" ]]; then
        perf script -i "${scen_out}/perf.data" > "/tmp/perf-${name}.script" \
            2>> "${scen_out}/perf.log" || log "warn: perf script failed"
        stackcollapse-perf.pl "/tmp/perf-${name}.script" > "${scen_out}/oncpu.folded" \
            2>> "${scen_out}/perf.log" || log "warn: collapse failed"
        flamegraph.pl --title "soldr on-CPU (${name})" \
            --subtitle "Linux Docker $(uname -r) / ${SAMPLE_HZ} Hz / cargo build via soldr" \
            "${scen_out}/oncpu.folded" > "${scen_out}/oncpu.svg" \
            2>> "${scen_out}/perf.log" || log "warn: flamegraph failed"
        log "on-CPU folded entries: $(wc -l < "${scen_out}/oncpu.folded" 2>/dev/null || echo 0)"
    else
        log "warn: no perf.data captured for ${name} — skipping on-CPU chart"
    fi

    log "==> rendering off-CPU flame chart"
    local rendered_offcpu=0
    if [[ -s "${offcpu_bt}" ]] && grep -q '@offcpu_us' "${offcpu_bt}"; then
        stackcollapse-bpftrace.pl "${offcpu_bt}" > "${scen_out}/offcpu.folded" \
            2>> "${scen_out}/offcpu.bpftrace.log" || log "warn: offcpu collapse failed"
        if [[ -s "${scen_out}/offcpu.folded" ]]; then
            flamegraph.pl --color=io --countname=us --title "soldr off-CPU (${name})" \
                --subtitle "blocked-stack µs (bpftrace) — Linux Docker $(uname -r)" \
                "${scen_out}/offcpu.folded" > "${scen_out}/offcpu.svg" \
                2>> "${scen_out}/offcpu.bpftrace.log" && rendered_offcpu=1
        fi
    fi
    if [[ "${rendered_offcpu}" -eq 0 && -s "${scen_out}/perf-sched.data" ]]; then
        log "==> bpftrace had no data — rendering perf-sched off-CPU fallback"
        perf script -i "${scen_out}/perf-sched.data" --no-inline > "/tmp/perf-sched-${name}.script" \
            2>> "${scen_out}/perf-sched.log" || log "warn: perf-sched script failed"
        stackcollapse-perf.pl --kernel "/tmp/perf-sched-${name}.script" \
            > "${scen_out}/offcpu.folded" \
            2>> "${scen_out}/perf-sched.log" || log "warn: perf-sched collapse failed"
        if [[ -s "${scen_out}/offcpu.folded" ]]; then
            flamegraph.pl --color=io --countname=switches --title "soldr off-CPU (${name})" \
                --subtitle "sched_switch stacks (perf sched fallback) — Linux Docker $(uname -r)" \
                "${scen_out}/offcpu.folded" > "${scen_out}/offcpu.svg" \
                2>> "${scen_out}/perf-sched.log" && rendered_offcpu=1
        fi
    fi
    if [[ "${rendered_offcpu}" -eq 1 ]]; then
        log "off-CPU folded entries: $(wc -l < "${scen_out}/offcpu.folded" 2>/dev/null || echo 0)"
    else
        log "warn: no off-CPU chart could be rendered for ${name}"
    fi

    log "==[done: ${name}]=="
}

for scen in ${SCENARIOS}; do
    run_scenario "${scen}"
done

log "==> all scenarios complete"
ls -la "${OUT_DIR}" | tee -a "${OUT_DIR}/run.log"
