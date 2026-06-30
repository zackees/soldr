#!/usr/bin/env bash
# Inside-the-container darwin cross-compile runner.
#
# Args:
#   $1 = target triple (x86_64-apple-darwin | aarch64-apple-darwin)
#   $@ = remaining args forwarded as cargo args (e.g. --release)
#
# Mirrors the env block in release-auto.yml's darwin Linux-host branch
# (post-v0.7.87). The whole point of this harness is that this same
# env, when applied locally inside a clean ubuntu-24.04 container,
# either:
#   (a) produces a Mach-O binary for soldr-cli → matches CI behavior
#       and proves the env is sufficient, OR
#   (b) hits the same compile/link error CI hits → reproducible bug we
#       can iterate on without waiting on a 30-min remote release run

set -euo pipefail

TARGET="${1:?usage: $0 <target-triple> [cargo args...]}"
shift

# Discover the extracted Apple SDK (top-level MacOSX*.sdk dir).
SDKROOT="$(ls -d /opt/apple-sdk/MacOSX*.sdk 2>/dev/null | head -1)"
if [ -z "$SDKROOT" ] || [ ! -f "$SDKROOT/usr/include/sys/syscall.h" ]; then
    echo "ERROR: no usable Apple SDK at /opt/apple-sdk/" >&2
    ls -la /opt/apple-sdk >&2 || true
    exit 1
fi
export SDKROOT
echo "Using SDKROOT=$SDKROOT"

# Diagnostic the v0.7.87 trace asked for: do we have the headers
# jemalloc needs?
for header in sys/syscall.h os/lock.h os/availability.h mach/mach.h; do
    if [ -f "$SDKROOT/usr/include/$header" ]; then
        echo "  ✓ $header"
    else
        echo "  ✗ $header (MISSING)"
    fi
done

case "$TARGET" in
    x86_64-apple-darwin)   CLANG_ARCH=x86_64-apple-darwin ;;
    aarch64-apple-darwin)  CLANG_ARCH=arm64-apple-darwin ;;
    *) echo "unsupported darwin target: $TARGET" >&2 ; exit 1 ;;
esac

TARGET_U="${TARGET//-/_}"
TARGET_U_UPPER="$(echo "$TARGET_U" | tr '[:lower:]' '[:upper:]')"

# v0.7.87's env block — keep in lockstep with release-auto.yml.
# -fuse-ld=lld in CFLAGS so cmake-rs sub-builds use lld for their
# test-compile-link step (else they call /usr/bin/ld which can't
# emit Mach-O).
export "CC_${TARGET_U}=clang --target=${CLANG_ARCH} -isysroot ${SDKROOT} -mmacosx-version-min=11.0 -fuse-ld=lld"
export "CXX_${TARGET_U}=clang++ --target=${CLANG_ARCH} -isysroot ${SDKROOT} -mmacosx-version-min=11.0 -stdlib=libc++ -fuse-ld=lld"
export "AR_${TARGET_U}=llvm-ar"
export "RANLIB_${TARGET_U}=llvm-ranlib"
export "CFLAGS_${TARGET_U}=-isysroot ${SDKROOT} -mmacosx-version-min=11.0 -fuse-ld=lld"
export "CXXFLAGS_${TARGET_U}=-isysroot ${SDKROOT} -mmacosx-version-min=11.0 -stdlib=libc++ -fuse-ld=lld"
export "CARGO_TARGET_${TARGET_U_UPPER}_LINKER=clang"
export "CARGO_TARGET_${TARGET_U_UPPER}_RUSTFLAGS=-C link-arg=--target=${CLANG_ARCH} -C link-arg=-isysroot -C link-arg=${SDKROOT} -C link-arg=-mmacosx-version-min=11.0 -C link-arg=-fuse-ld=lld"

echo "===== inline darwin env ====="
env | grep -E "^(SDKROOT|CC_|CXX_|AR_|RANLIB_|CFLAGS_|CXXFLAGS_|CARGO_TARGET_)" \
    | grep -E "${TARGET_U}|${TARGET_U_UPPER}|SDKROOT" \
    | sort
echo "============================="

cargo build --package soldr-cli --release --locked --target "$TARGET" "$@"

OUT="target/$TARGET/release/soldr"
echo "===== post-build ====="
if [ -f "$OUT" ]; then
    echo "OK — $(file "$OUT")"
else
    echo "MISSING binary at $OUT"
    ls -la "target/$TARGET/release/" 2>&1 | head -20 || true
    exit 1
fi
