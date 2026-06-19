#!/usr/bin/env bash
# Runs the README-facing bare cargo vs sccache vs soldr comparison
# benchmarks and emits benchmark-output/comparison.json.

set -euo pipefail

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${HERE}/.." && pwd)"

OUT_DIR="${REPO_ROOT}/benchmark-output"
WORK_DIR="$(mktemp -d)"
WORKTREES_TO_REMOVE=()
CREATED_PROJECT=""
trap 'for wt in "${WORKTREES_TO_REMOVE[@]:-}"; do git -C "${REPO_ROOT}" worktree remove --force "$wt" >/dev/null 2>&1 || true; done; rm -rf "${WORK_DIR}"' EXIT

mkdir -p "${OUT_DIR}"

# Keep setup-soldr's toolchain/PATH benefits, but remove workspace-pinned
# cache state so each comparison cell owns its cache and target dirs.
unset ZCCACHE_CACHE_DIR \
      SCCACHE_DIR \
      RUSTC_WRAPPER \
      SOLDR_RUSTC_WRAPPER \
      SOLDR_TARGET_CACHE_DIR \
      SOLDR_TARGET_CACHE_BUNDLE_DIR \
      SOLDR_TARGET_CACHE_MODE \
      SOLDR_TARGET_CACHE_PROFILE \
      SOLDR_TARGET_CACHE_BACKEND \
      SOLDR_TARGET_CACHE_COMPRESS \
      SOLDR_TARGET_CACHE_COMPRESS_LEVEL \
      SOLDR_TARGET_REGISTRY_RECORDED \
      SOLDR_BUILD_CACHE_MODE \
      SETUP_SOLDR_BUILD_CACHE_MODE

now_ms() { date +%s%3N; }

elapsed_ms() {
    local start="$1"
    local end
    end="$(now_ms)"
    echo "$(( end - start ))"
}

json_escape() {
    jq -Rn --arg v "$1" '$v'
}

append_result() {
    local benchmark="$1"
    local fixture="$2"
    local scenario_key="$3"
    local scenario="$4"
    local mode="$5"
    local tool="$6"
    local tool_label="$7"
    local command_text="$8"
    local wall_ms="$9"
    local project_path="${10}"

    cat >>"${RESULTS_JSONL}" <<EOF
{"benchmark":$(json_escape "$benchmark"),"fixture":$(json_escape "$fixture"),"scenario_key":$(json_escape "$scenario_key"),"scenario":$(json_escape "$scenario"),"mode":$(json_escape "$mode"),"tool":$(json_escape "$tool"),"tool_label":$(json_escape "$tool_label"),"command":$(json_escape "$command_text"),"wall_ms":${wall_ms},"project_path":$(json_escape "$project_path")}
EOF
}

run_logged() {
    local label="$1"
    shift
    echo "+ $label" >&2
    "$@" >&2
}

measure_command() {
    local label="$1"
    shift
    local t0 rc elapsed
    echo "::group::comparison ${label}" >&2
    echo "+ $*" >&2
    t0="$(now_ms)"
    set +e
    "$@" >&2
    rc=$?
    set -e
    elapsed="$(elapsed_ms "${t0}")"
    if (( rc == 0 )); then
        echo "comparison ${label}: ok (${elapsed} ms)" >&2
        echo "::endgroup::" >&2
        echo "${elapsed}"
    else
        echo "comparison ${label}: FAILED (rc=${rc}, ${elapsed} ms) - recording 0" >&2
        echo "::endgroup::" >&2
        echo "0"
    fi
}

median_ms() {
    printf '%s\n' "$@" | sort -n | awk 'NF { values[NR] = $1 } END { if (NR == 0) print 0; else print values[int((NR + 1) / 2)] }'
}

make_fixture_project() {
    local fixture="$1"
    local dest="$2"
    mkdir -p "${dest}"
    if [[ "${fixture}" == "soldr" ]]; then
        git -C "${REPO_ROOT}" worktree add --detach "${dest}/soldr" HEAD >/dev/null
        WORKTREES_TO_REMOVE+=("${dest}/soldr")
        CREATED_PROJECT="${dest}/soldr"
    else
        bash "${REPO_ROOT}/perf/lib/extract.sh" "${fixture}" "${dest}" >/dev/null
        CREATED_PROJECT="${dest}/${fixture}"
    fi
}

tool_label() {
    case "$1" in
        bare) echo "Bare cargo" ;;
        sccache) echo "sccache" ;;
        soldr) echo "soldr" ;;
        *) echo "$1" ;;
    esac
}

command_text() {
    case "$1" in
        bare) echo "cargo build --release --locked" ;;
        sccache) echo "RUSTC_WRAPPER=sccache cargo build --release --locked" ;;
        soldr) echo "soldr cargo build --release --locked" ;;
        *) echo "$1" ;;
    esac
}

run_build() {
    local tool="$1"
    local project="$2"
    local cache_dir="$3"
    local target_dir="$4"
    case "${tool}" in
        bare)
            (cd "${project}" && CARGO_TARGET_DIR="${target_dir}" cargo build --release --locked)
            ;;
        sccache)
            mkdir -p "${cache_dir}"
            (cd "${project}" && SCCACHE_DIR="${cache_dir}" RUSTC_WRAPPER=sccache CARGO_TARGET_DIR="${target_dir}" cargo build --release --locked)
            ;;
        soldr)
            mkdir -p "${cache_dir}"
            (cd "${project}" && SOLDR_CACHE_DIR="${cache_dir}" CARGO_TARGET_DIR="${target_dir}" soldr cargo build --release --locked)
            ;;
        *)
            echo "unknown tool: ${tool}" >&2
            return 2
            ;;
    esac
}

clean_project() {
    local project="$1"
    local target_dir="$2"
    (cd "${project}" && CARGO_TARGET_DIR="${target_dir}" cargo clean >/dev/null 2>&1 || true)
    rm -rf "${target_dir}"
}

measure_cold() {
    local benchmark="$1"
    local fixture="$2"
    local tool="$3"
    local samples=()
    for idx in 1 2 3; do
        local cell="${WORK_DIR}/${benchmark}/cold/${tool}/${idx}"
        local project cache target ms
        make_fixture_project "${fixture}" "${cell}/src"
        project="${CREATED_PROJECT}"
        cache="${cell}/cache"
        target="${cell}/target"
        ms="$(measure_command "${benchmark} cold ${tool} sample ${idx}" run_build "${tool}" "${project}" "${cache}" "${target}")"
        samples+=("${ms}")
    done
    median_ms "${samples[@]}"
}

measure_warm() {
    local benchmark="$1"
    local fixture="$2"
    local tool="$3"
    local cell="${WORK_DIR}/${benchmark}/warm/${tool}"
    local project cache target
    make_fixture_project "${fixture}" "${cell}/src"
    project="${CREATED_PROJECT}"
    cache="${cell}/cache"
    target="${cell}/target"
    run_logged "${benchmark} warm ${tool} populate" run_build "${tool}" "${project}" "${cache}" "${target}"
    clean_project "${project}" "${target}"
    measure_command "${benchmark} warm ${tool}" run_build "${tool}" "${project}" "${cache}" "${target}"
}

measure_worktree_share() {
    local benchmark="$1"
    local fixture="$2"
    local tool="$3"
    local cell="${WORK_DIR}/${benchmark}/worktree-share/${tool}"
    local project_a project_b cache target_a target_b
    make_fixture_project "${fixture}" "${cell}/src-a"
    project_a="${CREATED_PROJECT}"
    make_fixture_project "${fixture}" "${cell}/src-b"
    project_b="${CREATED_PROJECT}"
    cache="${cell}/cache"
    target_a="${cell}/target-a"
    target_b="${cell}/target-b"
    run_logged "${benchmark} worktree-share ${tool} populate A" run_build "${tool}" "${project_a}" "${cache}" "${target_a}"
    measure_command "${benchmark} worktree-share ${tool} build B" run_build "${tool}" "${project_b}" "${cache}" "${target_b}"
}

RESULTS_JSONL="${WORK_DIR}/comparison-results.jsonl"
: >"${RESULTS_JSONL}"

declare -A FIXTURES=(
    ["rust-only"]="soldr"
    ["rust-c"]="sqlite-link"
)

for benchmark in rust-only rust-c; do
    fixture="${FIXTURES[${benchmark}]}"
    for tool in bare sccache soldr; do
        label="$(tool_label "${tool}")"
        command="$(command_text "${tool}")"

        ms="$(measure_cold "${benchmark}" "${fixture}" "${tool}")"
        append_result "${benchmark}" "${fixture}" "cold" "Cold build" "cold" "${tool}" "${label}" "${command}" "${ms}" "median-of-3"

        ms="$(measure_warm "${benchmark}" "${fixture}" "${tool}")"
        append_result "${benchmark}" "${fixture}" "warm" "Warm rebuild (same workspace)" "warm" "${tool}" "${label}" "${command}" "${ms}" "single-sample"

        ms="$(measure_worktree_share "${benchmark}" "${fixture}" "${tool}")"
        append_result "${benchmark}" "${fixture}" "worktree-share" "Agent worktree / parent-child share" "worktree-share" "${tool}" "${label}" "${command}" "${ms}" "single-sample"
    done
done

ran_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
soldr_version="$(soldr --version 2>/dev/null || echo unknown)"
rustc_version="$(rustc --version 2>/dev/null || echo unknown)"
sccache_version="$(sccache --version 2>/dev/null || echo unknown)"

jq -s \
    --arg ran_at "${ran_at}" \
    --arg soldr_version "${soldr_version}" \
    --arg rustc_version "${rustc_version}" \
    --arg sccache_version "${sccache_version}" \
    '{
        ran_at: $ran_at,
        soldr_version: $soldr_version,
        rustc_version: $rustc_version,
        sccache_version: $sccache_version,
        schema_version: 1,
        scenarios: [
            {key: "cold", label: "Cold build", samples: 3},
            {key: "warm", label: "Warm rebuild (same workspace)", samples: 1},
            {key: "worktree-share", label: "Agent worktree / parent-child share", samples: 1}
        ],
        tools: [
            {key: "bare", label: "Bare cargo"},
            {key: "sccache", label: "sccache"},
            {key: "soldr", label: "soldr"}
        ],
        results: .
    }' "${RESULTS_JSONL}" >"${OUT_DIR}/comparison.json"

echo "comparison.json written to ${OUT_DIR}/comparison.json" >&2
cat "${OUT_DIR}/comparison.json" >&2
