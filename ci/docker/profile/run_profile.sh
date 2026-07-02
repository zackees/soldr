#!/usr/bin/env bash
# Entrypoint for the soldr Linux profiler image.
#
# soldr#1244+ update: this used to run three ad-hoc scenarios
# (`cold` / `warm` / `bare`) with an inline `soldr cargo build`. It
# now runs the FOUR authoritative perf-matrix scenarios defined in
# PERF.md against the `medium` fixture, wrapping each with the
# on-CPU (perf record) + off-CPU (bpftrace, perf-sched fallback)
# samplers via `scripts/run_scenario.sh`. When all four scenarios
# are done, `scripts/aggregate_top5.py` folds every scenario's
# `.folded` files into a single top-5 markdown table.
#
# Tokio-events profiling is deliberately out of scope for this
# iteration — soldr-daemon has no console-subscriber wiring today
# and adding it is a separate ~150-LOC PR.
#
# Outputs land in /out (host-side:
# .codex-artifacts/soldr-wider-perf-<STAMP>/).

set -euo pipefail

OUT_DIR=${OUT_DIR:-/out}
SAMPLE_HZ=${SOLDR_PROFILE_HZ:-99}
SCENARIOS=${SOLDR_PROFILE_SCENARIOS:-cold-tar-untar-warm worktree-share touch-no-change build-then-check}
FIXTURE_NAME=${SOLDR_PROFILE_FIXTURE_NAME:-medium}

export SAMPLE_HZ
export SOLDR_PROFILE_HZ="${SAMPLE_HZ}"

mkdir -p "${OUT_DIR}"

log() { echo "[$(date -u +%H:%M:%S)] $*" | tee -a "${OUT_DIR}/run.log"; }

log "=== soldr Linux profiler (perf-matrix cycle) ==="
log "sample rate  : ${SAMPLE_HZ} Hz"
log "scenarios    : ${SCENARIOS}"
log "fixture      : ${FIXTURE_NAME}"
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

log "==> installing rust toolchain 1.94.1 ..."
rustup toolchain install 1.94.1 --profile minimal --no-self-update 2>&1 \
    | tee -a "${OUT_DIR}/build.log" | tail -3
rustup default 1.94.1 2>&1 | tee -a "${OUT_DIR}/build.log" | tail -3

log "==> building soldr-cli + soldr-daemon (release + frame-pointers + debuginfo) ..."
# Release mode matters: the embedded zccache compile service runs
# inside soldr-daemon, so a debug-built daemon would underrun the
# baseline. Keep `-C force-frame-pointers` + `-C debuginfo=2` so perf
# still resolves clean stacks.
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

log "rustc        : $(command -v rustc) ($(rustc --version))"
log "cargo        : $(command -v cargo) ($(cargo --version))"

# --- fixture extraction --------------------------------------------
# perf-matrix scenarios each expect their own workspace (they use
# `git init` + one commit inside worktree-share, and `cargo clean` in
# touch-no-change), so extract the fixture once per scenario.
extract_fixture_for() {
    local scenario=$1
    local dest="/tmp/perf-workdir/${scenario}"
    rm -rf "${dest}"
    mkdir -p "${dest}"
    bash /work/perf/lib/extract.sh "${FIXTURE_NAME}" "${dest}" \
        >> "${OUT_DIR}/run.log" 2>&1
    # extract.sh drops the tree at <dest>/<fixture-name>/
    echo "${dest}/${FIXTURE_NAME}"
}

# --- scenario loop --------------------------------------------------
SCENARIO_LIST=()
for scen in ${SCENARIOS}; do
    SCENARIO_LIST+=("${scen}")
    scen_out="${OUT_DIR}/${scen}"
    mkdir -p "${scen_out}"

    fixture_path=$(extract_fixture_for "${scen}")
    log "==[fixture extracted -> ${fixture_path}]=="

    # `run_scenario.sh` wraps one scenario invocation with concurrent
    # on-CPU + off-CPU samplers and produces .folded + .svg pairs.
    if ! bash /usr/local/bin/run_scenario.sh "${scen}" "${fixture_path}" "${scen_out}"; then
        log "warn: run_scenario.sh exit non-zero for ${scen} (continuing)"
    fi
done

# --- top-5 aggregation ---------------------------------------------
log "==> aggregating top-5 slowest items across scenarios"
if ! python3 /usr/local/bin/aggregate_top5.py \
        --out-dir "${OUT_DIR}" \
        --scenarios "${SCENARIO_LIST[@]}" \
        --top-n 5 \
        2>&1 | tee -a "${OUT_DIR}/run.log"; then
    log "warn: aggregate_top5.py exit non-zero"
fi

log "==> all scenarios complete"
log "outputs:"
ls -la "${OUT_DIR}" | tee -a "${OUT_DIR}/run.log"
if [[ -f "${OUT_DIR}/top5.md" ]]; then
    log "--- top5.md ---"
    cat "${OUT_DIR}/top5.md" | tee -a "${OUT_DIR}/run.log"
    log "--- end top5.md ---"
fi
