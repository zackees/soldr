#!/usr/bin/env bash
# In-container entrypoint for the self-hosting cross-compile proof.
#
# 1. Build the seed soldr driver for the native linux-x86_64 host with
#    bare cargo (the one allowed bootstrap — no soldr exists yet).
# 2. Assert the CROSS toolchain (clang / lld / llvm-ar) is NOT on PATH,
#    so a successful cross build can only mean soldr provisioned it.
# 3. `soldr build --release --target <T>` — must self-provision LLVM
#    (clang-tool-chain-bins) + Apple SDK / xwin-cache + cmake/ninja.
# 4. Assert the produced binary's format matches the target.
#
# Usage: run.sh <target-triple>   (default aarch64-apple-darwin)
set -euo pipefail

target="${1:-aarch64-apple-darwin}"
echo "=================================================================="
echo "soldr#1309 self-hosting cross proof — target: $target"
echo "=================================================================="

# ---- 1. seed soldr driver (bare cargo, native host) --------------------
echo "::group::seed: cargo build -p soldr-cli (native x86_64-unknown-linux-gnu)"
# RUSTC_WRAPPER unset: the seed is the thing that will engage caching for
# everything downstream — no chicken-and-egg.
RUSTC_WRAPPER="" cargo build --release \
    --package soldr-cli --bin soldr \
    --target x86_64-unknown-linux-gnu
seed_dir="$(pwd)/target/x86_64-unknown-linux-gnu/release"
export PATH="$seed_dir:$PATH"
soldr --version
echo "::endgroup::"

# ---- 2. prove the cross toolchain is absent ----------------------------
echo "--- verifying NO host cross toolchain is installed ---"
missing_ok=1
for tool in clang clang++ lld ld.lld llvm-ar llvm-ranlib llvm-lib lld-link musl-gcc; do
    if command -v "$tool" >/dev/null 2>&1; then
        echo "UNEXPECTED: '$tool' is on PATH ($(command -v "$tool")) — the proof requires it absent" >&2
        missing_ok=0
    fi
done
if [ "$missing_ok" != 1 ]; then
    echo "ERROR: host cross toolchain present; cannot prove soldr self-provisions it" >&2
    exit 2
fi
echo "confirmed: clang / lld / llvm-ar / etc. are all absent from PATH"

# ---- 3. the self-hosting cross build -----------------------------------
echo "::group::soldr build --release --target $target"
soldr build --release --target "$target" --package soldr-cli --bin soldr
echo "::endgroup::"

# ---- 4. assert the artifact matches the target -------------------------
suffix=""
case "$target" in *-pc-windows-msvc) suffix=".exe" ;; esac
built="target/${target}/release/soldr${suffix}"
if [ ! -f "$built" ]; then
    echo "ERROR: expected binary not found at $built" >&2
    ls -la "target/${target}/release/" >&2 || true
    exit 1
fi
fmt="$(file -b "$built")"
echo "built: $built"
echo "format: $fmt"
case "$target" in
    *-pc-windows-msvc)  echo "$fmt" | grep -qiE "PE32|MS Windows" || { echo "NOT a PE binary" >&2; exit 1; } ;;
    *-apple-darwin)     echo "$fmt" | grep -qiE "Mach-O"          || { echo "NOT a Mach-O binary" >&2; exit 1; } ;;
    *-unknown-linux-*)  echo "$fmt" | grep -qiE "ELF"             || { echo "NOT an ELF binary" >&2; exit 1; } ;;
esac
echo "=================================================================="
echo "PASS: soldr self-provisioned the toolchain and built $target"
echo "=================================================================="
