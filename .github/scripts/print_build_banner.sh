#!/usr/bin/env bash
# Print a structured banner before every cargo build invocation so the
# workflow log shows what's being built, in what profile, against which
# target, and via which build tool (soldr cargo build / soldr cargo
# zigbuild / soldr cargo xwin build / bare cargo).
#
# Usage:
#   .github/scripts/print_build_banner.sh \
#       <package-or-workdir>   # e.g. "soldr-cli" or "crgx@v0.4.2"
#       <profile>              # "release" / "debug"
#       <target>               # e.g. "aarch64-pc-windows-msvc"
#       <tool>                 # "cargo" / "soldr cargo" / "soldr cargo zigbuild" / ...
#       [<manifest_path>]      # optional, e.g. "$work_dir/Cargo.toml"

set -euo pipefail

package="${1:?package required}"
profile="${2:?profile required}"
target="${3:?target required}"
tool="${4:?tool required}"
manifest="${5:-}"

# `rustc --version` and `soldr --version` are best-effort — when this
# banner runs before the bootstrap is on PATH (it doesn't today, but
# defensively), don't let an unresolved binary tank the step.
rustc_version="$(rustc --version 2>/dev/null || echo '(rustc not on PATH yet)')"
soldr_version="$(soldr --version 2>/dev/null || echo '(soldr not on PATH yet)')"

echo "==================================================================="
echo "build      : ${package}"
echo "profile    : ${profile}"
echo "target     : ${target}"
echo "tool       : ${tool}"
[ -n "${manifest}" ] && echo "manifest   : ${manifest}"
echo "host       : $(uname -srm)"
echo "rustc      : ${rustc_version}"
echo "soldr      : ${soldr_version}"
echo "started    : $(date -u +"%Y-%m-%dT%H:%M:%SZ")"
echo "==================================================================="
