#!/usr/bin/env bash
# Diagnose the two retained rust-cache producers without changing cache policy.
# This measures on-disk inputs before the action's post-job cleanup/compression;
# it is not a claim about the final compressed archive's byte allocation.
set -euo pipefail

target_triple="${1:?usage: cache_path_inventory.sh TARGET_TRIPLE}"
cargo_home="${CARGO_HOME:-$HOME/.cargo}"

echo "rust-cache input inventory (bytes; target=$target_triple)"
for path in \
  "$cargo_home/bin" \
  "$cargo_home/registry/cache" \
  "$cargo_home/registry/index" \
  "$cargo_home/registry/src" \
  "$cargo_home/git" \
  target/debug \
  target/release \
  "target/$target_triple/debug" \
  "target/$target_triple/release"; do
  if [[ -e "$path" ]]; then
    du -sb -- "$path"
  fi
done
