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
# 0. The fixture's C++ was compiled by the catalogue g++ — the compiler pin
#    (CXX_<triple> / CMAKE_CXX_COMPILER) is what keeps compile and link on
#    one stdlib; CXXSTDLIB is the belt-and-braces knob on top.
#    (cc-rs does not echo compiler command lines even under -vv; its env
#     resolution dump is the observable: CXX_<triple> = Some(<g++ path>).)
grep -E "CXX_${TARGET//-/_} = Some\(.*conda-linux-gnu-g\+\+" "$LOG" >/dev/null \
    || { echo "FAIL: cc-rs did not resolve the catalogue g++ for the C++ half" >&2; exit 1; }
echo "ok: cc-rs resolved the catalogue g++ (CXX_${TARGET//-/_})"
grep -E "CXX compiler identification is GNU" "$LOG" >/dev/null \
    || { echo "FAIL: CMake did not identify a GNU C++ compiler" >&2; exit 1; }
echo "ok: CMake configured with the catalogue g++ (GNU identification)"
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
# 4. RED half — reproduce the clud#858 mechanism. The vendored whisper-style
#    picker in build.rs sniffs zig signals and does NOT read CXXSTDLIB, so a
#    leaked ZIG_COMMAND makes it emit -lc++ while the catalogue GNU driver
#    (libstdc++ only) does the final link. That build MUST fail with the
#    exact downstream error; if it ever starts passing, either the sysroot
#    grew a libc++ (env pin story changed) or the fixture lost its teeth.
echo "== RED half: leaked zig signal must reproduce clud#858 =="
REDLOG="$(mktemp /tmp/cxx-stdlib-pin-red.XXXXXX.log)"
"$SOLDR_BIN" cargo clean --target "$TARGET" -p cxx-stdlib-pin 2>/dev/null || true
# --no-cache: the compile cache must not replay the green run's link
# artifact for this deliberately-broken variant (observed: with caching on,
# the changed `-l dylib=c++` link flag was served a stale cached link).
if ZIG_COMMAND=zig "$SOLDR_BIN" --no-cache build --target "$TARGET" >"$REDLOG" 2>&1; then
    echo "FAIL: build succeeded despite the zig-sniffed -lc++ on a libstdc++-only toolchain" >&2
    exit 1
fi
grep -E "cannot find -lc\+\+" "$REDLOG" >/dev/null \
    || { echo "FAIL: RED build failed for an unrelated reason:" >&2; tail -30 "$REDLOG" >&2; exit 1; }
echo "ok: leaked ZIG_COMMAND reproduces 'ld: cannot find -lc++' (clud#858 mechanism)"

echo "PASS: cxx-stdlib-pin acceptance ($TARGET)"
