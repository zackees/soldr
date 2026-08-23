#!/usr/bin/env bash
set -euo pipefail

mkdir -p /tools
cd /repo
CARGO_TARGET_DIR=/target soldr cargo build --locked -p soldr-cli --bin soldr
install -m 0755 /target/debug/soldr /tools/soldr
cp /tools/soldr /tools/soldr-daemon
chmod 0755 /tools/soldr-daemon
/tools/soldr version --json
