#!/bin/bash

set -euo pipefail

# Keep ephemeral executable shims beside the unoptimized test binaries. Docker
# mounts /target as a separate filesystem from /tmp; using /tmp would force
# five 100+ MiB copies and byte hashes per shim-dir test instead of hardlinks.
export TMPDIR="${CARGO_TARGET_DIR:-/target}/tmp"
mkdir -p "$TMPDIR"

# Bootstrap a truly empty target with the published Soldr baked into the
# image. Warm edits rebuild through the previous current-source binary so the
# RUSTC_WRAPPER identity remains stable and Cargo fingerprints stay warm.
current_soldr="${CARGO_TARGET_DIR:-/target}/debug/soldr"
if [[ -x "$current_soldr" ]]; then
  "$current_soldr" cargo build -p soldr-cli --bin soldr
else
  soldr cargo build -p soldr-cli --bin soldr
  soldr daemon stop || true
fi
export PATH="$(dirname "$current_soldr"):$PATH"

if [[ "${SOLDR_SMOKE_TOKIO_CONSOLE:-}" == "1" ]]; then
  export SOLDR_DAEMON_TOKIO_CONSOLE=1
  export SOLDR_DAEMON_TOKIO_CONSOLE_PUBLISH_INTERVAL_MS=20
  soldr daemon stop || true
  soldr daemon start
  CARGO_TARGET_DIR=/target/tokio-console-dump \
    soldr cargo build --manifest-path ci/tokio-console-dump/Cargo.toml
  diagnostics=/repo/target/diagnostics
  dump="$diagnostics/smoke-tokio-console.json"
  stop_file="$diagnostics/smoke-tokio-console.stop"
  mkdir -p "$diagnostics"
  rm -f "$stop_file"
  /target/tokio-console-dump/debug/soldr-tokio-console-dump \
    http://127.0.0.1:6669 7200000 "$dump" "$stop_file" &
  dump_pid=$!
  set +e
  bash ./test
  status=$?
  set -e
  touch "$stop_file"
  wait "$dump_pid" || true
  exit "$status"
fi

exec bash ./test
