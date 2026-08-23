#!/usr/bin/env bash
set -euo pipefail

fixture=/repo/tests/fixtures/cook-rematerialization
workspace=/workspace
rm -rf "$workspace" /artifact/* /root/.cargo/registry /root/.cargo/git
find /root/.soldr -mindepth 1 -maxdepth 1 -exec rm -rf -- {} +
mkdir -p "$workspace" /artifact /root/.cargo
cp -a "$fixture/." "$workspace/"
cd "$workspace"

started_ns="$(date +%s%N)"
ZCCACHE_DISABLE=1 /tools/soldr cook --release
finished_ns="$(date +%s%N)"
seed_ms="$(( (finished_ns - started_ns) / 1000000 ))"
printf '%s\n' "$seed_ms" > /artifact/seed-ms.txt

build_script="$(find target/release/build -type f -name 'build-script-build*' -print -quit)"
test -n "$build_script"
printf 'seed retained build script: %s\n' "$build_script"

/tools/soldr save \
  --cache-dir target \
  --workspace "$workspace" \
  --out /artifact/cook.tar.zst \
  --zstd-level 1 \
  --json | tee /artifact/seed-save.json
/tools/soldr save \
  --cache-dir /root/.cargo/registry \
  --out /artifact/cargo-registry.tar.zst \
  --zstd-level 1 \
  --json | tee /artifact/seed-registry-save.json

test -s /artifact/cook.tar.zst
test -s /artifact/cargo-registry.tar.zst
printf 'seed_ms=%s target_archive_bytes=%s registry_archive_bytes=%s\n' \
  "$seed_ms" \
  "$(stat -c %s /artifact/cook.tar.zst)" \
  "$(stat -c %s /artifact/cargo-registry.tar.zst)"
