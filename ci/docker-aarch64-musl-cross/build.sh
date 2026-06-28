#!/usr/bin/env bash
# Run inside the soldr-aarch64-musl-cross docker image.
# Cross-compiles the soldr binary to aarch64-unknown-linux-musl using
# the aarch64-linux-musl-cross toolchain pre-installed in the image.
#
# Output verification: asserts the produced binary is a valid
# ELF 64-bit LSB aarch64 executable. The NO CHEATING gate that
# proves the cross-compile actually worked.

set -euo pipefail

TARGET="${1:-aarch64-unknown-linux-musl}"
STAGING="${STAGING:-$PWD/staging}"
mkdir -p "$STAGING"

echo "::group::rustup + target install"
rustup toolchain install 1.94.1 --profile minimal --no-self-update
rustup default 1.94.1
rustup target add "$TARGET"
echo "::endgroup::"

echo "::group::env probe"
echo "PATH=$PATH"
echo "CC_aarch64_unknown_linux_musl=${CC_aarch64_unknown_linux_musl:-<unset>}"
echo "which aarch64-linux-musl-gcc: $(which aarch64-linux-musl-gcc || echo NOT-FOUND)"
echo "aarch64-linux-musl-gcc --version:"
aarch64-linux-musl-gcc --version | head -1
echo "::endgroup::"

echo "::group::Build soldr-cli for $TARGET"
# Skip the soldr wrapper — this is a fresh container without soldr
# bootstrapped — call cargo directly through rustup.
cargo build --release \
    --target "$TARGET" \
    -p soldr-cli \
    --bin soldr
echo "::endgroup::"

echo "::group::Stage + verify"
BIN="target/$TARGET/release/soldr"
if [ ! -f "$BIN" ]; then
    echo "ERROR: $BIN missing after build" >&2
    exit 1
fi
cp "$BIN" "$STAGING/soldr"
desc="$(file "$STAGING/soldr")"
echo "  $desc"
if ! echo "$desc" | grep -qE "ELF 64-bit LSB.*aarch64"; then
    echo "ERROR: expected ELF 64-bit aarch64 binary; got: $desc" >&2
    exit 1
fi
size=$(stat -c%s "$STAGING/soldr")
echo "  size: $size bytes ($((size / 1024 / 1024)) MiB)"
echo "::endgroup::"

echo
echo "OK: soldr cross-compiled to $TARGET successfully."
echo "Output: $STAGING/soldr"
