#!/usr/bin/env bash
# soldr#1105: run the msvc_host module tests under a Linux Docker
# container. Pure-function tests for the .cargo/config.toml rust-lld
# probe are Linux-runnable; the Windows-only end-to-end tests are
# cfg-gated and skip cleanly.
#
# Per CLAUDE.md "Agent Development Environment Rule" (issue #1105):
# develop and debug features in the local Docker Linux harness FIRST,
# then handle the Windows/cross-compile story afterwards.
#
# Subcommands:
#   ./scripts/test_msvc_host_linux.sh             # default: msvc_host tests
#   ./scripts/test_msvc_host_linux.sh clippy      # clippy on the whole crate
#   ./scripts/test_msvc_host_linux.sh shell       # drop into a bash shell

set -euo pipefail

here=$(cd "$(dirname "$0")/.." && pwd)

# Use the rust:1.95 base + install soldr's native build deps inline.
# Keep the image build cached so reruns are fast.
img="soldr-msvc-host-test:1.95"
docker build -t "$img" - <<'DOCKER_EOF'
FROM rust:1.95
RUN apt-get update && apt-get install -y --no-install-recommends \
    cmake perl pkg-config build-essential clang lld llvm \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /work
DOCKER_EOF

case "${1:-test}" in
    test)
        cmd='cargo test -p soldr-cli --lib msvc_host:: -- --nocapture 2>&1 | tail -200'
        ;;
    clippy)
        cmd='cargo clippy -p soldr-cli --lib -- -D warnings 2>&1 | tail -100'
        ;;
    shell)
        cmd='bash'
        ;;
    *)
        echo "unknown subcommand: $1" >&2
        echo "usage: $0 [test|clippy|shell]" >&2
        exit 64
        ;;
esac

# `-it` only when stdin is a TTY (interactive shells); CI / non-TTY
# callers need to pass `-i -t` selectively or the run errors with
# "the input device is not a TTY". Keep this script usable from both
# a developer terminal AND from CI / agent shells.
docker_tty_flags=""
if [ -t 0 ] && [ -t 1 ]; then
    docker_tty_flags="-it"
fi

# MSYS_NO_PATHCONV=1 prevents Git-for-Windows bash from mangling the
# /work container path into a host-side Windows path.
MSYS_NO_PATHCONV=1 docker run --rm $docker_tty_flags \
    -v "$here:/work" \
    -w //work \
    -e CARGO_TERM_COLOR=never \
    "$img" \
    bash -c "$cmd"
