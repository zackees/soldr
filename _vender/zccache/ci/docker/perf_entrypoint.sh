#!/usr/bin/env bash
# Runs inside the zccache-perf-runner container. Reproduces the per-cell
# bench job from .github/workflows/perf-rust-cluster.yml end-to-end on a
# pre-built pair of (soldr, zccache trio) binaries.
#
# Env contract (set by ci/perf_local.py):
#   SCENARIO  - cold-tar-untar-warm | worktree-share | touch-no-change
#   FIXTURE   - medium | sqlite-link
#
# Mount contract (all required, fail loud if missing):
#   /usr/local/bin/soldr    - the soldr binary (mode +x)
#   /zccache-bin/           - {zccache,zccache-daemon,zccache-fp}
#   /zccache-src/           - the zccache repo (read-only)
#   /results/               - host-writable; result.json + reports land here

set -euo pipefail

require_env() {
    local var="$1"
    if [[ -z "${!var:-}" ]]; then
        echo "ERROR: env var ${var} is required" >&2
        exit 2
    fi
}

require_mount() {
    local path="$1" kind="$2"
    if [[ ! -e "${path}" ]]; then
        echo "ERROR: required ${kind} not mounted at ${path}" >&2
        exit 2
    fi
}

require_env SCENARIO
require_env FIXTURE
require_mount /usr/local/bin/soldr file
require_mount /zccache-bin dir
require_mount /zccache-src dir
require_mount /results dir

# Confirm binaries are present + executable. The error here is friendlier
# than the later soldr-side failure if a binary is missing.
for bin in zccache zccache-daemon zccache-fp; do
    if [[ ! -x "/zccache-bin/${bin}" ]]; then
        echo "ERROR: /zccache-bin/${bin} missing or not executable" >&2
        exit 2
    fi
done

# The soldr binary is mounted read-only from the host's binaries/ dir.
# The builder image set +x on it at build time so no chmod is needed
# here (and chmod would fail on a read-only mount).
# `soldr update-zccache` expects a writable copy under its managed dir.
# We point it at /zccache-bin (read-only mount is fine; soldr copies out).
soldr update-zccache /zccache-bin --json | tee /results/zccache-pin.json
soldr update-zccache --status --json | tee /results/zccache-pin-status.json

# Same sanity check as .github/workflows/perf-rust-cluster.yml step
# "Pin zccache via soldr update-zccache". `.pinned` is an object in the
# current alias-backed subcommand; non-null + source_kind="path" means a
# local-path pin is in effect.
if ! jq -e '.pinned != null and .pinned.source_kind == "path"' \
        /results/zccache-pin-status.json >/dev/null; then
    echo "ERROR: soldr update-zccache reports no local-path pin after pinning" >&2
    cat /results/zccache-pin-status.json >&2
    exit 1
fi

# Work dir for the fixture extraction. The scenario scripts write under
# this dir and expect to own it. We use /tmp/perf-work so the persistent
# /results volume isn't polluted with intermediate state.
WORK_DIR="/tmp/perf-work-${SCENARIO}"
rm -rf "${WORK_DIR}"
mkdir -p "${WORK_DIR}"

# Step 1: extract the fixture tarball into ${WORK_DIR}/${FIXTURE}/
bash "/zccache-src/perf/lib/extract.sh" "${FIXTURE}" "${WORK_DIR}"

# Step 2: run the scenario. It owns its own cache-cold/ + cache-warm/
# subdirs under the parent of the fixture (i.e. ${WORK_DIR}/).
scenario_script="/zccache-src/perf/scenarios/${SCENARIO}/run.sh"
if [[ ! -f "${scenario_script}" ]]; then
    echo "ERROR: scenario script not found: ${scenario_script}" >&2
    exit 2
fi
bash "${scenario_script}" "${WORK_DIR}/${FIXTURE}" \
    | tee "/results/result.json"

# Step 3: copy the cache reports, shutdown reports, RSS CSV, and the
# per-session zccache logs that the GHA workflow's upload-artifact step
# also collects. Keeps the local results dir shape identical to what
# `gh api .../artifacts/<id>/zip` would land on disk, so the same
# downstream evaluator works on both.
copy_if_exists() {
    local src="$1"
    if [[ -e "${src}" ]]; then
        cp -R "${src}" /results/
    fi
}

scenario_root="${WORK_DIR}"
copy_if_exists "${scenario_root}/cold-cache-report.json"
copy_if_exists "${scenario_root}/warm-cache-report.json"
copy_if_exists "${scenario_root}/cold-shutdown.json"
copy_if_exists "${scenario_root}/warm-shutdown.json"
copy_if_exists "${scenario_root}/save-report.json"
copy_if_exists "${scenario_root}/load-report.json"
copy_if_exists "${scenario_root}/cold-zccache-logs"
copy_if_exists "${scenario_root}/warm-zccache-logs"
copy_if_exists "${scenario_root}/rss-${SCENARIO}.csv"

echo "DONE. Results in /results/:"
ls -la /results/
