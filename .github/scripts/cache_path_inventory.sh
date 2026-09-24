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

# rust-cache v2 keeps only build/, .fingerprint/, and deps/ under each
# profile. Its post-job cleanup also removes workspace crates and old files,
# so these are still upper bounds, not estimates of compressed cache bytes.
# Split the two release profiles to reveal whether a useful dependency tree
# or a disposable build product is responsible for the residual cache size.
for profile in target/release "target/$target_triple/release"; do
  [[ -d "$profile" ]] || continue
  echo "rust-cache profile components (bytes; profile=$profile)"
  for component in build .fingerprint deps incremental; do
    if [[ -d "$profile/$component" ]]; then
      du -sb -- "$profile/$component"
    fi
  done
  echo "rust-cache profile largest files (bytes; profile=$profile)"
  find "$profile" -type f -printf '%s\t%p\n' | sort -nr | sed -n '1,20p'
done
