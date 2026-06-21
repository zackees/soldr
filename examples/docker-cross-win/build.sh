#!/usr/bin/env bash
# Cross-compile the demo crate inside the Linux Docker image, copy the
# resulting Windows .exe back to the host, and (best-effort) smoke-test
# it on the host if it's runnable here.
#
# The point of this script is to be the one-command reproducer the
# example's README references. Layout:
#
#   examples/docker-cross-win/
#   ├── Dockerfile         # messense/cargo-zigbuild + windows-gnu target
#   ├── crate/             # tiny Rust source (only main.rs)
#   ├── build.sh           # this file
#   └── out/               # host-side artifact landing zone (gitignored)
#
# Usage:
#   ./build.sh                 # cross-compile + verify
#   ./build.sh --no-host-check # skip the host-side run (CI on linux)
#
# Exit codes:
#   0 — image built, exe produced, host check passed (or skipped)
#   2 — exe missing after the container run
#   3 — host check ran but the exe printed unexpected output
#   * — passed through from docker / cargo

set -euo pipefail

skip_host_check=0
if [ "${1:-}" = "--no-host-check" ]; then
    skip_host_check=1
fi

here=$(cd "$(dirname "$0")" && pwd)
crate_dir="$here/crate"
out_dir="$here/out"
img="soldr-docker-cross-win:demo"
target="x86_64-pc-windows-gnu"
bin_name="docker-cross-win-demo"

mkdir -p "$out_dir"

echo "==> docker build -t $img $here"
docker build -t "$img" "$here"

echo "==> cross-compile in container"
# Mount the crate dir as /work; the ENTRYPOINT runs `cargo zigbuild`
# with the CMD defaulting to --target $target --release.
#
# `MSYS_NO_PATHCONV=1` is the Git-for-Windows / MSYS bash convention
# that disables automatic path translation. Without it, MSYS rewrites
# `/work` (the container-side mount point) to `C:/Program Files/Git/work`
# on the way to Docker, which then fails to find the bind mount inside
# the container. POSIX hosts ignore the env var, so this is safe to set
# unconditionally.
MSYS_NO_PATHCONV=1 docker run --rm \
    -v "$crate_dir:/work" \
    "$img"

exe_in_crate="$crate_dir/target/$target/release/${bin_name}.exe"
if [ ! -f "$exe_in_crate" ]; then
    echo "ERROR: expected .exe not produced at $exe_in_crate" >&2
    echo "Contents of target/$target/release:" >&2
    ls -la "$crate_dir/target/$target/release/" 2>/dev/null >&2 || true
    exit 2
fi

cp "$exe_in_crate" "$out_dir/"
exe_out="$out_dir/${bin_name}.exe"

echo
echo "==> Artifact landed"
echo "    $exe_out"
size=$(stat --printf='%s' "$exe_out" 2>/dev/null || stat -f%z "$exe_out")
echo "    size:   ${size} bytes"
if command -v file >/dev/null 2>&1; then
    file "$exe_out" | sed 's/^/    type:   /'
fi
if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$exe_out" | awk '{print "    sha256: " $1}'
elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$exe_out" | awk '{print "    sha256: " $1}'
fi

if [ "$skip_host_check" = "1" ]; then
    echo
    echo "==> Skipping host-side run (--no-host-check)."
    exit 0
fi

# Host-side smoke test. Only meaningful when this script runs on
# Windows or under wine. On a plain linux host, we skip with a friendly
# note rather than failing.
host_can_run_pe=0
case "$(uname -s 2>/dev/null || echo unknown)" in
    MINGW*|MSYS*|CYGWIN*|Windows_NT|*NT*) host_can_run_pe=1 ;;
    *)
        if command -v wine >/dev/null 2>&1; then
            host_can_run_pe=1
        fi
        ;;
esac

if [ "$host_can_run_pe" = "0" ]; then
    echo
    echo "==> Host can't natively run a Windows PE; skipping run check."
    echo "    Re-run on a Windows host (or install wine on this linux box)"
    echo "    to exercise the end-to-end loop."
    exit 0
fi

echo
echo "==> Running the cross-compiled exe on the host"
host_runner=""
if [ "$(uname -s 2>/dev/null)" = "Linux" ] && command -v wine >/dev/null 2>&1; then
    host_runner="wine"
fi

if [ -n "$host_runner" ]; then
    output=$("$host_runner" "$exe_out" 2>&1)
else
    output=$("$exe_out" 2>&1)
fi
echo "$output" | sed 's/^/    /'

if ! printf '%s\n' "$output" | grep -q "docker-cross-win-demo OK"; then
    echo "ERROR: host run did not print the OK signature" >&2
    exit 3
fi
if ! printf '%s\n' "$output" | grep -q "target_os   = windows"; then
    echo "ERROR: host run did not report target_os = windows (got the above output)" >&2
    exit 3
fi

echo
echo "==> All checks passed."
echo "    Cross-compiled $bin_name.exe in Docker, exported to host, ran successfully."
