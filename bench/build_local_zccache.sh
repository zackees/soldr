#!/usr/bin/env bash
# Build a local zccache release with PDBs and print the soldr env-var
# command to point soldr at it.
#
# Usage:
#   bench/build_local_zccache.sh [path-to-zccache-checkout]
#
# Defaults to $HOME/dev/zccache. The zccache repo's
# [profile.release] should already ship `debug = "line-tables-only"`
# and `split-debuginfo = "packed"` so cargo build --release produces
# .exe + .pdb pairs in target/release.
set -euo pipefail

zccache_dir="${1:-$HOME/dev/zccache}"

if [ ! -d "$zccache_dir" ]; then
  echo "error: $zccache_dir is not a directory" >&2
  echo "hint: clone https://github.com/zackees/zccache there, or pass the path explicitly" >&2
  exit 1
fi
if [ ! -f "$zccache_dir/Cargo.toml" ]; then
  echo "error: $zccache_dir does not look like a Rust crate (no Cargo.toml)" >&2
  exit 1
fi

echo "soldr: building zccache release in $zccache_dir ..."
(cd "$zccache_dir" && cargo build --release)
target_release="$zccache_dir/target/release"

echo
echo "soldr: local zccache build complete. Point soldr at it with:"
echo
echo "  export SOLDR_ZCCACHE_LOCAL_DIR='$target_release'"
echo
echo "soldr: verify with: soldr doctor"
echo "soldr: the 'managed zccache' section should print 'source: local' and a 'symbol path' line."
