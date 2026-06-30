#!/usr/bin/env bash
# Inside-the-container darwin cross-compile runner.
#
# Args: $1 = target triple (x86_64-apple-darwin | aarch64-apple-darwin)
#       $2 = mode (default: soldr-build)
#         soldr-build  = production path: `cargo run --bin soldr -- build --target X`
#                        This is what release-auto.yml will call.
#                        blessed_build.rs surfaces the COMPLETE Apple SDK
#                        as cc-rs's CC/CXX/AR + rustc's linker.
#         cargo        = direct path: `cargo build --target X` with the
#                        SAME env block applied manually. Faster first-
#                        iteration; useful for debugging env tweaks
#                        without recompiling soldr.
#       remaining args forwarded as cargo args.
#
# Success criterion: `file target/<triple>/(debug|release)/soldr`
# reports `Mach-O 64-bit (x86_64|arm64) executable`.

set -euo pipefail

TARGET="${1:?usage: $0 <target-triple> [soldr-build|cargo] [args...]}"
MODE="${2:-soldr-build}"
shift 2 || shift 1 || true

export SDKROOT="${SDKROOT:-/opt/apple-sdk/MacOSX11.3.sdk}"
if [ ! -f "$SDKROOT/usr/include/sys/syscall.h" ]; then
    echo "ERROR: SDKROOT=$SDKROOT does not contain usr/include/sys/syscall.h" >&2
    echo "  The point of this harness is to use the COMPLETE Apple SDK." >&2
    exit 2
fi

case "$MODE" in
    soldr-build)
        # Production path: blessed_build.rs surfaces the env. We just
        # have to call `soldr build --target X`. Since the container
        # doesn't have a pre-built soldr binary, we `cargo run` it.
        echo "===== mode: soldr-build (production path via blessed_build.rs) ====="
        echo "SDKROOT=$SDKROOT (will be re-resolved by apple_sdk fetcher inside soldr)"
        exec cargo run --bin soldr -- build --target "$TARGET" "$@"
        ;;
    cargo)
        # Direct path: apply the env block manually (mirrors what
        # blessed_build.rs will do). Useful for iterating on the env
        # without recompiling soldr.
        TARGET_U="${TARGET//-/_}"
        TARGET_U_UPPER="$(echo "$TARGET_U" | tr '[:lower:]' '[:upper:]')"
        case "$TARGET" in
            x86_64-apple-darwin)   CLANG_TARGET=x86_64-apple-darwin ;;
            aarch64-apple-darwin)  CLANG_TARGET=arm64-apple-darwin ;;
            *) echo "ERROR: unsupported target $TARGET" >&2 ; exit 2 ;;
        esac
        CLANG_FLAGS="--target=$CLANG_TARGET -isysroot $SDKROOT -mmacosx-version-min=11.0"

        export CC_${TARGET_U}="clang $CLANG_FLAGS"
        export CXX_${TARGET_U}="clang++ $CLANG_FLAGS -stdlib=libc++"
        export AR_${TARGET_U}="llvm-ar"
        export RANLIB_${TARGET_U}="llvm-ranlib"
        export CFLAGS_${TARGET_U}="-isysroot $SDKROOT -mmacosx-version-min=11.0"
        export CXXFLAGS_${TARGET_U}="-isysroot $SDKROOT -mmacosx-version-min=11.0 -stdlib=libc++"
        export CARGO_TARGET_${TARGET_U_UPPER}_LINKER="clang"
        export CARGO_TARGET_${TARGET_U_UPPER}_RUSTFLAGS="\
          -C link-arg=--target=$CLANG_TARGET \
          -C link-arg=-isysroot \
          -C link-arg=$SDKROOT \
          -C link-arg=-mmacosx-version-min=11.0 \
          -C link-arg=-fuse-ld=lld"

        echo "===== mode: cargo (direct path, env applied manually) ====="
        env | grep -E "^(SDKROOT|CC_|CXX_|AR_|RANLIB_|CFLAGS_|CXXFLAGS_|CARGO_TARGET_)" \
            | grep -E "${TARGET_U}|${TARGET_U_UPPER}|SDKROOT" | sort
        echo "================================================================"

        exec cargo build --target "$TARGET" --package soldr-cli "$@"
        ;;
    *)
        echo "ERROR: unknown mode '$MODE' (expected: soldr-build | cargo)" >&2
        exit 2
        ;;
esac
