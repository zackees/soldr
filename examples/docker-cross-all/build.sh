#!/usr/bin/env bash
# One-command reproducer: cross-compile the demo crate for all 8
# targets in `[workspace.metadata.soldr].targets` from a single
# linux x86 docker image, then report per-target binary size and the
# `target/` partitioning across triples.
#
# Replaces the per-OS GH Actions matrix with a local-docker step
# that arrives warm — `soldr prepare --target all` is baked into the
# image at build time, so the first `docker run` does zero network
# fetches and goes straight into `cargo build` for every triple.
#
# Usage:
#   ./build.sh                  # build all 8 targets
#   ./build.sh --target X       # build a single triple
#   ./build.sh --no-build-image # reuse the existing image tag
#
# Exit codes:
#   0  — every requested target built; artifacts copied to out/<triple>/
#   2  — expected binary missing for some target (lookup printed)
#   64 — bad CLI arg

set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
img="soldr-docker-cross-all:demo"
crate_dir="$here/crate"
out_dir="$here/out"
bin_name="docker-cross-all-demo"

ALL_TARGETS=(
    "x86_64-pc-windows-msvc"
    "aarch64-pc-windows-msvc"
    "x86_64-apple-darwin"
    "aarch64-apple-darwin"
    "x86_64-unknown-linux-gnu"
    "aarch64-unknown-linux-gnu"
    "x86_64-unknown-linux-musl"
    "aarch64-unknown-linux-musl"
)

build_image=1
selected_targets=()
while [ $# -gt 0 ]; do
    case "$1" in
        --target)
            shift
            if [ $# -eq 0 ]; then
                echo "ERROR: --target requires a value" >&2
                exit 64
            fi
            selected_targets+=("$1")
            ;;
        --target=*)
            selected_targets+=("${1#--target=}")
            ;;
        --no-build-image)
            build_image=0
            ;;
        -h|--help)
            sed -n 's/^# \{0,1\}//;1,/^$/p' "$0"
            exit 0
            ;;
        *)
            echo "ERROR: unrecognized argument: $1" >&2
            exit 64
            ;;
    esac
    shift
done

if [ "${#selected_targets[@]}" -eq 0 ]; then
    selected_targets=("${ALL_TARGETS[@]}")
fi

mkdir -p "$out_dir"

if [ "$build_image" = "1" ]; then
    echo "==> docker build -t $img $here"
    docker build --platform linux/amd64 -t "$img" "$here"
fi

# All targets share one target/ — we run cargo from the same /work/crate
# bind mount across every docker run, which keeps the per-triple
# subdirectories (target/<triple>/) co-located so cache save/load can
# treat them as a single artifact in later iterations.

echo "==> cross-build pass: ${#selected_targets[@]} targets"
echo

for t in "${selected_targets[@]}"; do
    echo "----------------------------------------------------------------------"
    echo "==> target: $t"
    start=$(date +%s)
    docker run --rm --platform linux/amd64 \
        -v "$crate_dir:/work" \
        -w /work \
        "$img" \
        -c "soldr cargo build --release --target $t"
    elapsed=$(( $(date +%s) - start ))

    triple_release="$crate_dir/target/$t/release"
    bin_path=""
    # Try common output shapes — windows is .exe, others are bare.
    for candidate in \
        "$triple_release/${bin_name}.exe" \
        "$triple_release/${bin_name}"; do
        if [ -f "$candidate" ]; then
            bin_path="$candidate"
            break
        fi
    done
    if [ -z "$bin_path" ]; then
        echo "ERROR: no binary produced under $triple_release" >&2
        ls -la "$triple_release" 2>/dev/null >&2 || true
        exit 2
    fi

    out_sub="$out_dir/$t"
    mkdir -p "$out_sub"
    cp "$bin_path" "$out_sub/"
    out_bin="$out_sub/$(basename "$bin_path")"

    bin_size=$(stat --printf='%s' "$out_bin" 2>/dev/null || stat -f%z "$out_bin")
    triple_target_size=$(du -sh "$crate_dir/target/$t" 2>/dev/null | awk '{print $1}')
    incr_dir="$crate_dir/target/$t/release/incremental"
    incr_size=""
    if [ -d "$incr_dir" ]; then
        incr_size=$(du -sh "$incr_dir" 2>/dev/null | awk '{print $1}')
    fi
    file_type=$(file "$out_bin" 2>/dev/null || echo unknown)

    printf '    elapsed:   %ss\n' "$elapsed"
    printf '    bin size:  %s bytes\n' "$bin_size"
    printf '    bin type:  %s\n' "$file_type"
    printf '    target/%s: %s\n' "$t" "$triple_target_size"
    if [ -n "$incr_size" ]; then
        printf '    incr/:     %s\n' "$incr_size"
    fi
    echo
done

echo "======================================================================"
echo "==> Final target/ partition report (cargo per-triple layout)"
du -sh "$crate_dir/target" 2>/dev/null || true
du -sh "$crate_dir/target/release" 2>/dev/null || true
for t in "${selected_targets[@]}"; do
    if [ -d "$crate_dir/target/$t" ]; then
        du -sh "$crate_dir/target/$t" 2>/dev/null
    fi
done

echo
echo "==> All artifacts under: $out_dir"
echo "==> Build summary by triple:"
for t in "${selected_targets[@]}"; do
    if [ -d "$out_dir/$t" ]; then
        for f in "$out_dir/$t"/*; do
            sz=$(stat --printf='%s' "$f" 2>/dev/null || stat -f%z "$f")
            printf '    %-30s  %12s  %s\n' "$t" "${sz} bytes" "$(basename "$f")"
        done
    fi
done
