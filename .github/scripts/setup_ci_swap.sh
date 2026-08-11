#!/usr/bin/env bash
# Enlarge swap on GitHub-hosted Linux runners to absorb transient LLVM/LTO
# memory peaks during all-miss cross-compiles (soldr#2453).
#
# The 4-vCPU ubuntu-24.04 image ships ~16 GB RAM and only a ~4 GB swapfile.
# A cold `codegen-units=1` thin-LTO build of the daemon crates
# (`zccache-daemon-core`, `soldr-daemon`) plus the resident embedded-zccache
# daemon can momentarily exceed that ceiling and get OOM-killed. The kill
# surfaces as "compiler process was terminated by a Unix signal" and has
# moved between unrelated crates run-to-run — the signature of memory
# pressure, not a compile error.
#
# Bounding Cargo's producer queue (CARGO_BUILD_JOBS) and zccache's admission
# gate (SOLDR_JOBS) reduced the failure rate but did not eliminate it: the
# floor is a single memory-heavy rustc/LLVM child colliding with the daemon's
# resident state. Adding a large swapfile gives that transient peak somewhere
# to spill instead of tripping the OOM killer.
#
# Swap is a safety valve, not a hot path: with a low swappiness the kernel
# keeps working pages in RAM and only touches swap under genuine pressure, so
# the common (fits-in-RAM) case is unaffected while the pathological peak no
# longer kills the build.
set -euo pipefail

SIZE_GB="${CI_SWAP_GB:-14}"
# /mnt is the large ephemeral SSD on GitHub-hosted runners (~70 GB free);
# the OS disk (/) is much smaller, so the swapfile lives on /mnt.
SWAPFILE="/mnt/ci-extra-swapfile"

echo "== swap before =="
free -h || true
swapon --show || true

# Replace any existing swap with a single large file so total swap is
# deterministic rather than "default 4 GB plus ours".
sudo swapoff -a || true
sudo rm -f "$SWAPFILE"

# fallocate is instant on ext4; fall back to dd if the filesystem rejects it.
if ! sudo fallocate -l "${SIZE_GB}G" "$SWAPFILE" 2>/dev/null; then
  sudo dd if=/dev/zero of="$SWAPFILE" bs=1M count=$((SIZE_GB * 1024)) status=none
fi
sudo chmod 600 "$SWAPFILE"
sudo mkswap "$SWAPFILE" >/dev/null
sudo swapon "$SWAPFILE"

# Keep pages in RAM until real pressure hits (10 = spill late, stay fast).
sudo sysctl -w vm.swappiness=10 >/dev/null || true

echo "== swap after =="
free -h || true
swapon --show || true
