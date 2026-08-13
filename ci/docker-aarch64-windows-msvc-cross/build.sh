#!/usr/bin/env bash
# Run inside the soldr-aarch64-windows-msvc-cross docker image.
# Cross-compiles the soldr binary to `aarch64-pc-windows-msvc` from
# x86_64 Linux using `soldr build` (the blessed surface that installs
# soldr-managed clang shims ahead of system clang).
#
# Every soldr / cargo invocation is routed through run_with_ts.py so
# each output line carries an elapsed-seconds prefix. This is the
# observability lever that GHA run 28345283384 / job 83967287913
# lacked — the build went silent for 30 minutes before hitting the
# 1800-second soldr cargo diagnostic timeout, with zero per-line
# timing data to localize where it actually stalled.
#
# Output verification: asserts the produced binary is a valid
# `PE32+ executable (console) Aarch64 ... Windows` file. The NO
# CHEATING gate that proves the cross-compile actually worked.

set -euo pipefail

TARGET="${1:-aarch64-pc-windows-msvc}"
STAGING="${STAGING:-$PWD/staging}"
mkdir -p "$STAGING"

WRAPPER="${WRAPPER:-python3 /src/.github/scripts/run_with_ts.py}"

echo "::group::rustup + target install"
$WRAPPER rustup toolchain install 1.95.0 --profile minimal --no-self-update
$WRAPPER rustup default 1.95.0
$WRAPPER rustup target add "$TARGET"
echo "::endgroup::"

echo "::group::env probe"
echo "PATH=$PATH"
echo "soldr version: $(soldr --version)"
echo "rustc version: $(rustc --version)"
echo "cargo version: $(cargo --version)"
echo "clang version: $(clang --version | head -n1)"
echo "cargo-xwin version: $(cargo-xwin --version)"
ls -la /opt/soldr/ || true
echo "::endgroup::"

echo "::group::Build soldr-cli for $TARGET via soldr build (blessed surface)"
# `soldr build --target ...` is the v0.7.66+ blessed surface. It
# installs clang shim names ahead of system clang on PATH so ring 0.17.x's
# hardcoded `c.compiler("clang")` (build.rs:563) routes to clang-cl for
# *-pc-windows-msvc targets. cargo-xwin is also installed
# in the image; soldr can dispatch to it for the SDK download.
$WRAPPER soldr build --release \
    --target "$TARGET" \
    -p soldr-cli \
    --bin soldr
echo "::endgroup::"

echo "::group::Stage + verify"
BIN="target/$TARGET/release/soldr.exe"
if [ ! -f "$BIN" ]; then
    echo "ERROR: $BIN missing after build" >&2
    ls -la "target/$TARGET/release/" >&2 || true
    exit 1
fi
cp "$BIN" "$STAGING/soldr.exe"
desc="$(file "$STAGING/soldr.exe")"
echo "  $desc"
if ! echo "$desc" | grep -qE "PE32\+.*Aarch64|PE32\+.*ARM64|MS Windows.*ARM"; then
    echo "ERROR: expected PE32+ Aarch64 / ARM64 Windows binary; got: $desc" >&2
    exit 1
fi
size=$(stat -c%s "$STAGING/soldr.exe")
echo "  size: $size bytes ($((size / 1024 / 1024)) MiB)"
echo "::endgroup::"

echo
echo "OK: soldr cross-compiled to $TARGET successfully."
echo "Output: $STAGING/soldr.exe"
