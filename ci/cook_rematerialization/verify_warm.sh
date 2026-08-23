#!/usr/bin/env bash
set -euo pipefail

fixture=/repo/tests/fixtures/cook-rematerialization
workspace=/workspace
rm -rf "$workspace" /root/.cargo/registry /root/.cargo/git
find /root/.soldr -mindepth 1 -maxdepth 1 -exec rm -rf -- {} +
mkdir -p "$workspace" /root/.cargo
cp -a "$fixture/." "$workspace/"
cd "$workspace"

test -s /artifact/cook.tar.zst
test -s /artifact/cargo-registry.tar.zst
/tools/soldr load \
  --archive /artifact/cargo-registry.tar.zst \
  --cache-dir /root/.cargo/registry \
  --json | tee /artifact/warm-registry-load.json
/tools/soldr load \
  --archive /artifact/cook.tar.zst \
  --cache-dir target \
  --workspace "$workspace" \
  --json | tee /artifact/warm-load.json

build_script="$(find target/release/build -type f -name 'build-script-build*' -print -quit)"
test -n "$build_script"
printf 'warm restored build script: %s\n' "$build_script"

started_ns="$(date +%s%N)"
ZCCACHE_DISABLE=1 /tools/soldr cargo build \
  --release \
  --locked \
  -vv \
  --message-format=json-render-diagnostics \
  > /artifact/warm-messages.jsonl \
  2> /artifact/warm-stderr.log
finished_ns="$(date +%s%N)"
warm_ms="$(( (finished_ns - started_ns) / 1000000 ))"
printf '%s\n' "$warm_ms" > /artifact/warm-ms.txt
printf '%s\n' "$warm_ms" >> /artifact/warm-samples.txt

actual="$(target/release/cook-rematerialization-fixture)"
test "$actual" = '{"value":42}'
python3 /repo/ci/cook_rematerialization/assert_warm.py \
  /artifact/warm-messages.jsonl \
  /artifact/warm-stderr.log \
  /artifact/seed-ms.txt \
  /artifact/warm-ms.txt \
  /artifact/warm-samples.txt
