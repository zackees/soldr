#!/usr/bin/env bash
# Evidence harness for soldr issue #1551:
#   perf(cargo): evaluate checksum freshness for touch-without-content-change
#
# Runs a scenario matrix over three cargo modes on the `medium` perf fixture:
#   stable-mtime     : cargo 1.94.1 (repo-pinned), stable mtime freshness
#   nightly-mtime    : cargo nightly, mtime freshness (compiler-controlled baseline)
#   nightly-checksum : cargo nightly + -Zchecksum-freshness
#
# Scenarios per mode (each mode gets its own CARGO_TARGET_DIR, cold-started):
#   cold          full build from empty target dir
#   noop x3       nothing changed
#   touch-main x3 `touch src/main.rs` (mtime bumped, bytes unchanged)
#   touch-all x3  touch src/main.rs build.rs build_input.txt
#   samestat x1   1-byte content mutation of src/main.rs, size preserved,
#                 mtime restored -> MUST rebuild under checksum mode
#   edit x3       true content edit (append a fn) to src/main.rs
#   bs-touch x3   touch build_input.txt (rerun-if-changed input, bytes unchanged)
#   bs-samestat   1-byte same-size same-mtime mutation of build_input.txt
#
# Every build runs with RUSTC_WRAPPER set to a counting shim so we get exact
# rustc-invocation counts (the soldr-relevant "wrapper count"). One extra rep
# of each mutating scenario runs with CARGO_LOG=cargo::core::compiler::fingerprint=info
# to capture the exact Dirty reasons.
#
# Usage (inside the soldr-perf-local container):
#   bash harness-1551.sh /target/issue-1551-fx /repo/perf
set -euo pipefail

ROOT="${1:?usage: harness-1551.sh <workdir> <perf-dir>}"
PERF="${2:?usage: harness-1551.sh <workdir> <perf-dir>}"
FIXTURE=medium
SRC="$ROOT/$FIXTURE"
LOGD="$ROOT/logs"
WRAP="$ROOT/count-rustc.sh"
RESULTS="$LOGD/results.txt"

mkdir -p "$LOGD"

# ---------------------------------------------------------------- fixture prep
prep_fixture() {
    rm -rf "$SRC"
    mkdir -p "$ROOT/extract"
    rm -rf "$ROOT/extract"
    mkdir -p "$ROOT/extract"
    tar -C "$ROOT/extract" -xzf "$PERF/fixtures/$FIXTURE.tar.gz"
    mv "$ROOT/extract/$FIXTURE" "$SRC"
    # Deterministic 1-byte mutation targets.
    printf '// mutation pad: 0\n' >> "$SRC/src/main.rs"
    printf '0\n' > "$SRC/build_input.txt"
    cat > "$SRC/build.rs" <<'EOF'
fn main() {
    println!("cargo:rerun-if-changed=build_input.txt");
    let v = std::fs::read_to_string("build_input.txt").unwrap();
    println!("cargo:rustc-env=BUILD_INPUT_V={}", v.trim());
}
EOF
    cp "$SRC/src/main.rs" "$ROOT/main.rs.pristine"
    cp "$SRC/build_input.txt" "$ROOT/build_input.pristine"
}

restore_pristine() {
    cp "$ROOT/main.rs.pristine" "$SRC/src/main.rs"
    cp "$ROOT/build_input.pristine" "$SRC/build_input.txt"
}

# ------------------------------------------------------------- counting shim
cat > "$WRAP" <<'EOF'
#!/bin/sh
echo "$@" >> "${RUSTC_COUNT_FILE:-/dev/null}"
exec "$@"
EOF
chmod +x "$WRAP"

# ------------------------------------------------------------------ mode glue
mode_cmd() {
    case "$1" in
        stable-mtime) echo "cargo" ;;
        *) echo "rustup run nightly cargo" ;;
    esac
}
mode_flags() {
    case "$1" in
        nightly-checksum) echo "-Zchecksum-freshness" ;;
        *) echo "" ;;
    esac
}

# build <mode> <label> <fplog:0|1>
build() {
    local mode="$1" label="$2" fplog="${3:-0}"
    local tgt="$ROOT/tgt-$mode"
    local cnt="$LOGD/$mode.$label.count"
    : > "$cnt"
    local cmd flags start end ms n status=0
    cmd=$(mode_cmd "$mode")
    flags=$(mode_flags "$mode")
    start=$(date +%s%N)
    if [ "$fplog" = 1 ]; then
        ( cd "$SRC" && \
          CARGO_LOG=cargo::core::compiler::fingerprint=info \
          RUSTC_COUNT_FILE="$cnt" RUSTC_WRAPPER="$WRAP" CARGO_TARGET_DIR="$tgt" \
          $cmd build $flags ) > "$LOGD/$mode.$label.out" 2> "$LOGD/$mode.$label.fplog" || status=$?
    else
        ( cd "$SRC" && \
          RUSTC_COUNT_FILE="$cnt" RUSTC_WRAPPER="$WRAP" CARGO_TARGET_DIR="$tgt" \
          $cmd build $flags ) > "$LOGD/$mode.$label.out" 2> "$LOGD/$mode.$label.err" || status=$?
    fi
    end=$(date +%s%N)
    ms=$(( (end - start) / 1000000 ))
    n=$(wc -l < "$cnt")
    echo "RESULT mode=$mode label=$label wall_ms=$ms rustc_invocations=$n status=$status" | tee -a "$RESULTS"
}

stat_line() { # evidence: size + ns-precision mtime
    stat -c 'STAT %n size=%s mtime=%y' "$1" | tee -a "$RESULTS"
}

# --------------------------------------------------------------- one mode run
run_mode() {
    local mode="$1"
    echo "=== MODE $mode ===" | tee -a "$RESULTS"
    restore_pristine
    rm -rf "$ROOT/tgt-$mode"

    build "$mode" cold 0

    for i in 1 2 3; do build "$mode" "noop-$i" 0; done
    build "$mode" noop-logged 1

    for i in 1 2 3; do
        touch "$SRC/src/main.rs"
        build "$mode" "touch-main-$i" 0
    done
    touch "$SRC/src/main.rs"
    build "$mode" touch-main-logged 1

    for i in 1 2 3; do
        touch "$SRC/src/main.rs" "$SRC/build.rs" "$SRC/build_input.txt"
        build "$mode" "touch-all-$i" 0
    done

    # -- samestat: 1-byte mutation, same size, same mtime (the false-Fresh trap)
    cp -p "$SRC/src/main.rs" "$ROOT/main.rs.ref"
    stat_line "$SRC/src/main.rs"
    sed -i 's|// mutation pad: 0|// mutation pad: 1|' "$SRC/src/main.rs"
    touch -r "$ROOT/main.rs.ref" "$SRC/src/main.rs"
    stat_line "$SRC/src/main.rs"
    build "$mode" samestat-main 1
    # restore (mtime bumps naturally) + stabilize
    cp "$ROOT/main.rs.pristine" "$SRC/src/main.rs"
    build "$mode" stabilize-1 0

    for i in 1 2 3; do
        printf 'fn _edit_%s_%s() {}\n' "$mode" "$i" | tr - _ >> "$SRC/src/main.rs"
        build "$mode" "edit-$i" 0
    done

    for i in 1 2 3; do
        touch "$SRC/build_input.txt"
        build "$mode" "bs-touch-$i" 0
    done
    touch "$SRC/build_input.txt"
    build "$mode" bs-touch-logged 1

    # -- build-script input samestat
    cp -p "$SRC/build_input.txt" "$ROOT/bi.ref"
    stat_line "$SRC/build_input.txt"
    printf '1\n' > "$SRC/build_input.txt"
    touch -r "$ROOT/bi.ref" "$SRC/build_input.txt"
    stat_line "$SRC/build_input.txt"
    build "$mode" bs-samestat 1
    cp "$ROOT/build_input.pristine" "$SRC/build_input.txt"
    build "$mode" stabilize-2 0
}

# ------------------------------------------------------------------- main
: > "$RESULTS"
echo "harness start: $(date -u +%FT%TZ)" | tee -a "$RESULTS"
( cargo --version; rustup run nightly cargo --version ) | tee -a "$RESULTS"

prep_fixture

run_mode stable-mtime
run_mode nightly-mtime
run_mode nightly-checksum

# Sanity probe: env-var spelling of the unstable flag (no CLI flag needed).
restore_pristine
touch "$SRC/src/main.rs"
( cd "$SRC" && \
  RUSTC_COUNT_FILE="$LOGD/envvar.count" RUSTC_WRAPPER="$WRAP" \
  CARGO_TARGET_DIR="$ROOT/tgt-nightly-checksum" \
  CARGO_UNSTABLE_CHECKSUM_FRESHNESS=true \
  rustup run nightly cargo build ) > "$LOGD/envvar.out" 2> "$LOGD/envvar.err" \
  && echo "RESULT envvar-opt-in=works rustc_invocations=$(wc -l < "$LOGD/envvar.count")" | tee -a "$RESULTS" \
  || echo "RESULT envvar-opt-in=FAILED status=$?" | tee -a "$RESULTS"

echo "harness end: $(date -u +%FT%TZ)" | tee -a "$RESULTS"
