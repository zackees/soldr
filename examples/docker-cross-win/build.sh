#!/usr/bin/env bash
# Cross-compile the demo crate inside the Linux Docker image, copy the
# resulting Windows .exe back to the host, and (best-effort) smoke-test
# it on the host if the architectures match.
#
# The point of this script is to be the one-command reproducer the
# example's README references. Layout:
#
#   examples/docker-cross-win/
#   ├── Dockerfile         # messense/cargo-zigbuild + 2 win targets
#   ├── crate/             # tiny Rust source (only main.rs)
#   ├── build.sh           # this file
#   └── out/               # host-side artifact landing zone (gitignored)
#
# Usage:
#   ./build.sh                                       # default x86_64
#   ./build.sh --target aarch64-pc-windows-gnullvm   # windows ARM64
#   ./build.sh --no-host-check                       # skip the run check
#   ./build.sh --target <t> --no-host-check          # combined
#
# Exit codes:
#   0  — image built, exe produced, host check passed (or correctly skipped)
#   2  — exe missing after the container run
#   3  — host check ran but the exe printed unexpected output
#   4  — file(1) reports a PE arch that doesn't match the target
#   64 — bad CLI argument

set -euo pipefail

# Default: the canonical Microsoft ABI for Windows desktop x64.
# Matches soldr's "MSVC on Windows always" design rule (CLAUDE.md).
# The GNU and gnullvm lanes remain selectable via --target.
target="x86_64-pc-windows-msvc"
skip_host_check=0
while [ $# -gt 0 ]; do
    case "$1" in
        --target)
            shift
            if [ $# -eq 0 ]; then
                echo "ERROR: --target requires a value" >&2
                exit 64
            fi
            target="$1"
            ;;
        --target=*)
            target="${1#--target=}"
            ;;
        --no-host-check)
            skip_host_check=1
            ;;
        -h|--help)
            sed -n 's/^# \{0,1\}//;1,/^$/p' "$0"
            exit 0
            ;;
        *)
            echo "ERROR: unrecognized argument: $1" >&2
            echo "Try --help." >&2
            exit 64
            ;;
    esac
    shift
done

# Map the target triple to:
#   - the architecture string `file(1)` reports inside the PE header,
#     used for the post-build arch sanity check
#   - the cross-link toolchain we dispatch to. MSVC-ABI targets need
#     cargo-xwin (clang-cl + lld-link + the MSVC SDK); the GNU/gnullvm
#     family uses cargo-zigbuild (zig as the linker).
case "$target" in
    x86_64-pc-windows-gnu*|x86_64-pc-windows-gnullvm)
        expected_pe_arch="x86-64"; cross_tool="zigbuild" ;;
    x86_64-pc-windows-msvc)
        expected_pe_arch="x86-64"; cross_tool="xwin" ;;
    i686-pc-windows-gnu*|i686-pc-windows-gnullvm)
        expected_pe_arch="Intel 80386"; cross_tool="zigbuild" ;;
    i686-pc-windows-msvc)
        expected_pe_arch="Intel 80386"; cross_tool="xwin" ;;
    aarch64-pc-windows-gnullvm)
        expected_pe_arch="ARM64"; cross_tool="zigbuild" ;;
    aarch64-pc-windows-msvc)
        expected_pe_arch="ARM64"; cross_tool="xwin" ;;
    *)
        echo "ERROR: unsupported target $target." >&2
        echo "Supported by this script:" >&2
        echo "  x86_64-pc-windows-gnu          (zigbuild)" >&2
        echo "  aarch64-pc-windows-msvc        (xwin)" >&2
        echo "  aarch64-pc-windows-gnullvm     (zigbuild)" >&2
        echo "(other windows triples may work too — extend the Dockerfile's" >&2
        echo "rustup target list and add a case here mapping to expected_pe_arch" >&2
        echo "and either 'zigbuild' or 'xwin'.)" >&2
        exit 64
        ;;
esac

here=$(cd "$(dirname "$0")" && pwd)
crate_dir="$here/crate"
out_dir="$here/out"
img="soldr-docker-cross-win:demo"
bin_name="docker-cross-win-demo"

mkdir -p "$out_dir"

echo "==> docker build -t $img $here"
docker build -t "$img" "$here"

echo "==> cross-compile in container for target: $target  (via cargo-$cross_tool)"
# Mount the crate dir as /work; the ENTRYPOINT is plain `cargo`, so we
# pass the subcommand + flags as the CMD override.
#
# `MSYS_NO_PATHCONV=1` is the Git-for-Windows / MSYS bash convention
# that disables automatic path translation. Without it, MSYS rewrites
# `/work` (the container-side mount point) to `C:/Program Files/Git/work`
# on the way to Docker, which then fails to find the bind mount inside
# the container. POSIX hosts ignore the env var, so this is safe to set
# unconditionally.
#
# Mount the xwin SDK cache so the ~700 MB MSVC CRT download survives
# `docker run --rm`. The first xwin invocation downloads the SDK; every
# subsequent one is offline.
xwin_cache="$here/.xwin-cache"
if [ "$cross_tool" = "xwin" ]; then
    mkdir -p "$xwin_cache"
    MSYS_NO_PATHCONV=1 docker run --rm \
        -v "$crate_dir:/work" \
        -v "$xwin_cache:/root/.cache/cargo-xwin" \
        "$img" \
        xwin build --target "$target" --release
else
    MSYS_NO_PATHCONV=1 docker run --rm \
        -v "$crate_dir:/work" \
        "$img" \
        zigbuild --target "$target" --release
fi

exe_in_crate="$crate_dir/target/$target/release/${bin_name}.exe"
if [ ! -f "$exe_in_crate" ]; then
    echo "ERROR: expected .exe not produced at $exe_in_crate" >&2
    echo "Contents of target/$target/release:" >&2
    ls -la "$crate_dir/target/$target/release/" 2>/dev/null >&2 || true
    exit 2
fi

# Per-target landing path so x86_64 and aarch64 artifacts coexist.
out_subdir="$out_dir/$target"
mkdir -p "$out_subdir"
cp "$exe_in_crate" "$out_subdir/"
exe_out="$out_subdir/${bin_name}.exe"

echo
echo "==> Artifact landed"
echo "    $exe_out"
size=$(stat --printf='%s' "$exe_out" 2>/dev/null || stat -f%z "$exe_out")
echo "    size:   ${size} bytes"
file_output=""
if command -v file >/dev/null 2>&1; then
    file_output=$(file "$exe_out")
    printf '    type:   %s\n' "$file_output"
fi
if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$exe_out" | awk '{print "    sha256: " $1}'
elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$exe_out" | awk '{print "    sha256: " $1}'
fi

# Arch sanity check: the PE header should match what the target triple
# promised. Without this, a misconfigured Dockerfile or stale build
# cache could silently ship the wrong architecture.
if [ -n "$file_output" ]; then
    case "$file_output" in
        *"$expected_pe_arch"*) ;;
        *)
            echo "ERROR: produced .exe is not the expected $expected_pe_arch architecture." >&2
            echo "       file(1) said: $file_output" >&2
            exit 4
            ;;
    esac
    echo "    ✓ PE arch matches target ($expected_pe_arch)"
fi

if [ "$skip_host_check" = "1" ]; then
    echo
    echo "==> Skipping host-side run (--no-host-check)."
    exit 0
fi

# Host-side smoke test. Two conditions must hold:
#   1. The host can run Windows PEs (native Windows, MSYS bash, or wine).
#   2. The PE's architecture matches the host's architecture, since
#      native Windows on x64 cannot execute an ARM64 PE (and vice
#      versa) without WoW64-style emulation that this script doesn't
#      try to detect.
host_can_run_pe=0
case "$(uname -s 2>/dev/null || echo unknown)" in
    MINGW*|MSYS*|CYGWIN*|Windows_NT|*NT*) host_can_run_pe=1 ;;
    *)
        if command -v wine >/dev/null 2>&1; then
            host_can_run_pe=1
        fi
        ;;
esac

# Normalize the host arch to the same labels we used above for the PE
# arch. `uname -m` prints `x86_64` / `aarch64` / `arm64` / `i686`.
host_arch_label="unknown"
case "$(uname -m 2>/dev/null || echo unknown)" in
    x86_64|amd64) host_arch_label="x86-64" ;;
    aarch64|arm64) host_arch_label="ARM64" ;;
    i?86) host_arch_label="Intel 80386" ;;
esac

if [ "$host_can_run_pe" = "0" ]; then
    echo
    echo "==> Host can't natively run a Windows PE; skipping run check."
    echo "    Re-run on a Windows host (or install wine on this linux box)"
    echo "    to exercise the end-to-end loop."
    exit 0
fi

if [ "$expected_pe_arch" != "$host_arch_label" ]; then
    echo
    echo "==> Host arch ($host_arch_label) ≠ PE arch ($expected_pe_arch); skipping run check."
    echo "    The cross-compile is proven by the PE-arch verification above —"
    echo "    we just can't natively execute the binary on this machine."
    echo "    Re-run on a $expected_pe_arch host to exercise the end-to-end loop."
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
echo "    Cross-compiled $bin_name.exe ($target) in Docker, exported to host, ran successfully."
