#!/usr/bin/env bash
# soldr#1043 / #1038 phase 5: persistent zig-cc wrapper scripts for
# darwin cross-compile.
#
# Background: `cargo nextest archive` runs `cargo test --no-run` which
# re-invokes cc-rs for ring's darwin asm. cargo-zigbuild's CC/AR/linker
# wrappers persist only inside the `cargo zigbuild` invocation, not into
# a downstream `cargo nextest` step. The result is "linux cc rejects
# `-arch x86_64`" failures at archive time.
#
# This script materializes persistent wrapper scripts on disk and prints
# the env-var exports the caller should source. Calling pattern from a
# GHA step:
#
#     eval "$(bash .github/scripts/make_zig_cc_wrappers.sh "$target")"
#
# Then any downstream step in the same job sees the CC_<target> /
# CXX_<target> / AR_<target> / CARGO_TARGET_<T>_LINKER env vars.
#
# Usage:
#   make_zig_cc_wrappers.sh <target-triple> [--out-dir <path>]
#
# Args:
#   <target-triple>   e.g. aarch64-apple-darwin, x86_64-apple-darwin
#   --out-dir <path>  where to write the wrappers (default: $RUNNER_TEMP/zig-wrappers)
set -euo pipefail

if [ "$#" -lt 1 ]; then
    echo "usage: $0 <target-triple> [--out-dir <path>]" >&2
    exit 2
fi

target="$1"
shift
out_dir="${RUNNER_TEMP:-/tmp}/zig-wrappers"
while [ "$#" -gt 0 ]; do
    case "$1" in
        --out-dir)
            out_dir="$2"
            shift 2
            ;;
        *)
            echo "unknown arg: $1" >&2
            exit 2
            ;;
    esac
done

# Map target → zig --target spec. zig's CPU/OS naming differs from rust's
# triple format. For apple-darwin zig wants `aarch64-macos` / `x86_64-macos`.
case "$target" in
    aarch64-apple-darwin)
        zig_target="aarch64-macos"
        ;;
    x86_64-apple-darwin)
        zig_target="x86_64-macos"
        ;;
    aarch64-unknown-linux-musl)
        zig_target="aarch64-linux-musl"
        ;;
    x86_64-unknown-linux-musl)
        zig_target="x86_64-linux-musl"
        ;;
    aarch64-unknown-linux-gnu)
        zig_target="aarch64-linux-gnu"
        ;;
    x86_64-unknown-linux-gnu)
        zig_target="x86_64-linux-gnu"
        ;;
    *)
        echo "unsupported target for zig-cc wrappers: $target" >&2
        exit 2
        ;;
esac

mkdir -p "$out_dir"

cc_path="$out_dir/${target}-cc"
cxx_path="$out_dir/${target}-c++"
ar_path="$out_dir/${target}-ar"
linker_path="$out_dir/${target}-linker"

cat > "$cc_path" <<EOF
#!/usr/bin/env bash
# Auto-generated persistent zig-cc wrapper for $target (soldr#1043).
# Strip caller --target=<rust-triple>: cc-rs injects it for cross
# builds, but zig's -target (set below) uses zig-format triples and
# rejects rust-format like aarch64-unknown-linux-gnu with
# "UnknownOperatingSystem". soldr#1068.
filtered=()
for arg in "\$@"; do
    case "\$arg" in
        --target=*) ;;
        *) filtered+=("\$arg") ;;
    esac
done
exec zig cc -target $zig_target "\${filtered[@]}"
EOF
chmod +x "$cc_path"

cat > "$cxx_path" <<EOF
#!/usr/bin/env bash
# Auto-generated persistent zig-c++ wrapper for $target (soldr#1043).
# See CC wrapper for --target filtering rationale.
filtered=()
for arg in "\$@"; do
    case "\$arg" in
        --target=*) ;;
        *) filtered+=("\$arg") ;;
    esac
done
exec zig c++ -target $zig_target "\${filtered[@]}"
EOF
chmod +x "$cxx_path"

cat > "$ar_path" <<EOF
#!/usr/bin/env bash
# Auto-generated persistent zig-ar wrapper for $target (soldr#1043).
exec zig ar "\$@"
EOF
chmod +x "$ar_path"

cat > "$linker_path" <<EOF
#!/usr/bin/env bash
# Auto-generated persistent zig-cc linker wrapper for $target (soldr#1043).
# See CC wrapper for --target filtering rationale. The duplicate-_start
# problem from #1068 is handled at the workflow level via the
# CARGO_TARGET_<T>_RUSTFLAGS=-C link-self-contained=no env var
# (so rustc doesn't pass its own crt files), NOT via -nostartfiles
# here (zig appears to ignore -nostartfiles in -target mode).
filtered=()
for arg in "\$@"; do
    case "\$arg" in
        --target=*) ;;
        *) filtered+=("\$arg") ;;
    esac
done
exec zig cc -target $zig_target "\${filtered[@]}"
EOF
chmod +x "$linker_path"

# Build the env-var name suffix per cc-rs / cargo convention.
target_u=$(echo "$target" | tr '-' '_')
target_u_upper=$(echo "$target_u" | tr '[:lower:]' '[:upper:]')

cat <<EOF
export CC_${target_u}=$cc_path
export CXX_${target_u}=$cxx_path
export AR_${target_u}=$ar_path
export CARGO_TARGET_${target_u_upper}_LINKER=$linker_path
EOF

# Also emit GITHUB_ENV-style entries when running under GHA so the
# vars persist into downstream steps.
if [ -n "${GITHUB_ENV:-}" ]; then
    {
        echo "CC_${target_u}=$cc_path"
        echo "CXX_${target_u}=$cxx_path"
        echo "AR_${target_u}=$ar_path"
        echo "CARGO_TARGET_${target_u_upper}_LINKER=$linker_path"
    } >> "$GITHUB_ENV"
fi
