#!/usr/bin/env bash
# soldr#2309 acceptance: a C++-using crate (cc-rs + CMake, whisper-rs-sys
# pattern) cross-built for aarch64-unknown-linux-gnu through `soldr build`
# must link against libstdc++ with NO `-lc++` on any link line.
#
# Runs INSIDE an existing Linux dev container (per the repo's Agent
# Development Environment rule) — e.g. the recycled perf-local runner:
#
#   uv run --no-project python ci/perf_local.py cargo build --release -p soldr-cli
#   docker exec -w /repo <soldr-perf-local-...> bash ci/cxx_stdlib_pin_acceptance.sh
#
# Env:
#   SOLDR_BIN  path to the soldr binary under test
#              (default: target/release/soldr under the checkout root)
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET="aarch64-unknown-linux-gnu"
FIXTURE="$REPO_ROOT/ci/fixtures/cxx-stdlib-pin"
# The perf-local runner exports CARGO_TARGET_DIR=/target; honor it for both
# the soldr binary under test and the fixture's own build products.
TARGET_ROOT="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
SOLDR_BIN="${SOLDR_BIN:-$TARGET_ROOT/release/soldr}"
LOG="$(mktemp /tmp/cxx-stdlib-pin.XXXXXX.log)"

[ -x "$SOLDR_BIN" ] || { echo "ERROR: soldr binary not found at $SOLDR_BIN (set SOLDR_BIN)" >&2; exit 2; }
echo "== soldr under test: $("$SOLDR_BIN" --version)"

# The pin is setdefault — make sure this proof runs the injected default.
unset CXXSTDLIB TARGET_CXXSTDLIB "CXXSTDLIB_${TARGET//-/_}" 2>/dev/null || true

echo "== cross-building fixture for $TARGET (verbose) =="
cd "$FIXTURE"
# Force a real rebuild so the verbose link lines the assertions grep for are
# actually emitted (a warm no-op build would make the -lc++ check vacuous).
"$SOLDR_BIN" cargo clean --target "$TARGET" -p cxx-stdlib-pin 2>/dev/null || true
"$SOLDR_BIN" build --target "$TARGET" -vv 2>&1 | tee "$LOG"

echo "== assertions =="
# 1. No -lc++ on any compile/link line (word-bounded: -lstdc++ must not match).
if grep -E '(^|[ "=])-lc\+\+($|[ "])' "$LOG"; then
    echo "FAIL: a link line requested -lc++ (LLVM libc++)" >&2
    exit 1
fi
echo "ok: no -lc++ on any link line"

# 2. cc-rs resolved the pinned stdlib: its metadata line names stdc++.
if ! grep -q 'rustc-link-lib=stdc++' "$LOG"; then
    echo "WARN: no explicit rustc-link-lib=stdc++ line found (cc-rs may have linked it another way)" >&2
fi

# 3. The produced ELF carries libstdc++ (dynamic NEEDED or, as the conda
#    GNU driver does by default, statically embedded) and never libc++.
BIN="$TARGET_ROOT/$TARGET/debug/cxx-stdlib-pin"
[ -f "$BIN" ] || { echo "FAIL: expected binary missing at $BIN" >&2; exit 1; }
READELF="$(command -v readelf || true)"
if [ -z "$READELF" ]; then
    READELF="$(find "${HOME}/.soldr" -name '*-readelf' -type f 2>/dev/null | head -n1)"
fi
[ -n "$READELF" ] || { echo "FAIL: no readelf available to inspect $BIN" >&2; exit 1; }
DYN="$("$READELF" -d "$BIN")"
if echo "$DYN" | grep -E 'NEEDED.*libc\+\+'; then
    echo "FAIL: binary needs LLVM libc++" >&2
    exit 1
fi
if echo "$DYN" | grep -qE 'NEEDED.*libstdc\+\+'; then
    echo "ok: ELF dynamically links libstdc++"
elif "$READELF" -sW "$BIN" | grep '_ZNSt' >/dev/null; then
    # (grep without -q drains readelf's full output — `grep -q` exits at the
    #  first match, readelf takes SIGPIPE, and pipefail turns that into 141.)
    # The catalogue driver's default: libstdc++ linked statically, so the
    # C++ runtime symbols live in the binary itself and nothing is NEEDED.
    echo "ok: libstdc++ statically embedded (std:: symbols present, no libc++ NEEDED)"
else
    echo "FAIL: no evidence of libstdc++ (neither NEEDED nor embedded std:: symbols):" >&2
    echo "$DYN" >&2
    exit 1
fi
echo "PASS: cxx-stdlib-pin acceptance ($TARGET)"
