#!/usr/bin/env bash
# Repro harness for soldr issue #1579:
#   perf(rust-plan): add build_script_build to thin-v1 allowlist
#
# Repro: save -> delete target -> restore -> build, on the `medium` perf
# fixture, comparing thin-v1 (rlib/rmeta retained) BEFORE and AFTER adding
# `build_script_build` to the thin-v1 artifact-class allowlist.
#
# Usage (inside the soldr-perf-local container):
#   SOLDR_BIN=/target/issue-1579/debug/soldr bash repro-1579.sh /target/issue-1579-repro /repo/.claude/issue-1579
set -euo pipefail

ROOT="${1:?usage: repro-1579.sh <workdir> <repo-dir>}"
REPO="${2:?usage: repro-1579.sh <workdir> <repo-dir>}"
SOLDR_BIN="${SOLDR_BIN:?set SOLDR_BIN to the built soldr binary path}"
FIXTURE=medium
PERF="${REPO}/perf"

rm -rf "$ROOT"
mkdir -p "$ROOT"
tar -C "$ROOT" -xzf "$PERF/fixtures/$FIXTURE.tar.gz"
SRC="$ROOT/$FIXTURE"

CACHE_DIR="$ROOT/soldr-cache"
BUNDLE_DIR="$ROOT/bundle"
TARGET_DIR="$ROOT/target"
mkdir -p "$CACHE_DIR" "$BUNDLE_DIR"

export SOLDR_TRUST_INHERITED_ENV=1
export SOLDR_CACHE_DIR="$CACHE_DIR"
export SOLDR_TARGET_CACHE_MODE=thin
export SOLDR_TARGET_CACHE_BUNDLE_DIR="$BUNDLE_DIR"
export SOLDR_TARGET_CACHE_PROFILE="${PROFILE:-thin-v1}"
export CARGO_TARGET_DIR="$TARGET_DIR"

echo "=== profile: $SOLDR_TARGET_CACHE_PROFILE ==="

echo "--- cold build (populates target + saves bundle) ---"
( cd "$SRC" && "$SOLDR_BIN" cargo build --locked ) > "$ROOT/cold.out" 2> "$ROOT/cold.err" || {
    echo "cold build FAILED"; tail -60 "$ROOT/cold.err"; exit 1;
}

echo "--- bundle size after save ---"
du -sh "$BUNDLE_DIR" | tee "$ROOT/bundle-size.txt"

echo "--- delete target ---"
rm -rf "$TARGET_DIR"

echo "--- restore + build (measured) ---"
( cd "$SRC" && CARGO_LOG=cargo::core::compiler::fingerprint=info \
  "$SOLDR_BIN" cargo build --locked -v ) > "$ROOT/warm.out" 2> "$ROOT/warm.err" || {
    echo "warm build FAILED"; tail -60 "$ROOT/warm.err"; exit 1;
}

fresh=$(grep -c 'Fresh ' "$ROOT/warm.out" || true)
compiling=$(grep -c '^\s*Compiling ' "$ROOT/warm.out" || true)
total=$(( fresh + compiling ))
stale_dep_fp=$(grep -c 'StaleDepFingerprint' "$ROOT/warm.err" || true)

echo "RESULT profile=$SOLDR_TARGET_CACHE_PROFILE fresh=$fresh compiling=$compiling total=$total stale_dep_fingerprint_hits=$stale_dep_fp" | tee "$ROOT/result.txt"
echo "bundle_size: $(cat "$ROOT/bundle-size.txt")"
