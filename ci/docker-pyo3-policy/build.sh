#!/usr/bin/env bash
set -euo pipefail

ROOT=/src
POLICY_ROOT="$ROOT/ci/docker-pyo3-policy"
FIXTURE="$POLICY_ROOT/fixtures/abi3-extension"
NO_PYO3_FIXTURE="$POLICY_ROOT/fixtures/no-pyo3"
OUT=/tmp/soldr-pyo3-policy
export CARGO_TARGET_DIR=/target
export SOLDR_CACHE_DIR=/root/.soldr

mkdir -p "$OUT/native" "$OUT/windows" "$OUT/darwin-x64" "$OUT/darwin-arm64"

soldr toolchain ensure
soldr cargo build -p soldr-cli --bin soldr
SOLDR="$CARGO_TARGET_DIR/debug/soldr"
test -x "$SOLDR"
export PATH="$(dirname "$SOLDR"):$PATH"

"$SOLDR" rustup target add \
    x86_64-pc-windows-msvc \
    x86_64-apple-darwin \
    aarch64-apple-darwin

echo "::group::native PEP 517 hook + import"
PYTHONPATH="$ROOT/src" python3 - "$FIXTURE" "$OUT/native" <<'PY'
import importlib
import os
import pathlib
import sys
import zipfile

import soldr

fixture = pathlib.Path(sys.argv[1])
out = pathlib.Path(sys.argv[2])
os.chdir(fixture)
wheel = out / soldr.build_wheel(str(out))
unpacked = out / "unpacked"
with zipfile.ZipFile(wheel) as archive:
    archive.extractall(unpacked)
sys.path.insert(0, str(unpacked))
module = importlib.import_module("soldr_pyo3_policy_fixture")
assert module.answer() == 42
print(f"native import OK: {wheel.name}")
PY
echo "::endgroup::"

echo "::group::no-PyO3 Windows build"
"$SOLDR" build --manifest-path "$NO_PYO3_FIXTURE/Cargo.toml" \
    --target x86_64-pc-windows-msvc
echo "::endgroup::"

build_extension() {
    local target="$1"
    local artifact="$2"
    echo "::group::ABI3 extension $target"
    "$SOLDR" build --release \
        --manifest-path "$FIXTURE/Cargo.toml" \
        --target "$target"
    test -f "$artifact"
    file "$artifact"
    echo "::endgroup::"
}

build_extension x86_64-pc-windows-msvc \
    "$CARGO_TARGET_DIR/x86_64-pc-windows-msvc/release/soldr_pyo3_policy_fixture.dll"
build_extension x86_64-apple-darwin \
    "$CARGO_TARGET_DIR/x86_64-apple-darwin/release/libsoldr_pyo3_policy_fixture.dylib"
build_extension aarch64-apple-darwin \
    "$CARGO_TARGET_DIR/aarch64-apple-darwin/release/libsoldr_pyo3_policy_fixture.dylib"

if [ -d "$SOLDR_CACHE_DIR/bin/syslib/python" ]; then
    echo "unexpected Python compatibility sysroot materialized" >&2
    find "$SOLDR_CACHE_DIR/bin/syslib/python" -maxdepth 4 -type f >&2
    exit 1
fi

echo "PyO3 target-aware Docker policy validation OK"
