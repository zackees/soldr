#!/usr/bin/env bash
set -euo pipefail

: "${SOLDR_RELEASE_DRIVER:?set SOLDR_RELEASE_DRIVER to the pinned soldr executable}"
: "${SOLDR_RELEASE_TOOLCHAIN:?set SOLDR_RELEASE_TOOLCHAIN to the pinned Rust channel}"

exec "$SOLDR_RELEASE_DRIVER" rustup run "$SOLDR_RELEASE_TOOLCHAIN" cargo "$@"
