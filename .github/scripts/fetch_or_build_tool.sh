#!/usr/bin/env bash
# fetch_or_build_tool.sh - Try the soldr-toolchain catalogue first; fall back to
# a source build only on a miss.
#
# Used by `cross-compile-all-targets.yml` to install `crgx` and
# `cargo-chef` into the per-target soldr distribution archive. The
# fast path is a single curl from the LFS-aware CDN; the fallback
# preserves the old behavior (git clone + soldr cargo (xwin|zigbuild)
# build).
#
# USAGE: fetch_or_build_tool.sh <tool> <target-triple> <build-tool> <exe-suffix> <dest-dir>
#
#   tool         crgx | cargo-chef
#   target       e.g. x86_64-unknown-linux-gnu
#   build-tool   zigbuild | xwin | "" (native)
#   exe-suffix   .exe (windows) or "" (unix)
#   dest-dir     destination directory for the binary
#
# The script exports a per-tool `<TOOL>_SOURCE_COMMIT` to $GITHUB_ENV
# when it source-builds (so the downstream manifest.json writer keeps
# carrying a real commit sha). On the fast path the value is the
# string "prebuilt".

set -euo pipefail

CURL_CONNECT_TIMEOUT_SECS="${FETCH_OR_BUILD_TOOL_CURL_CONNECT_TIMEOUT_SECS:-10}"
CURL_MAX_TIME_SECS="${FETCH_OR_BUILD_TOOL_CURL_MAX_TIME_SECS:-120}"
GIT_CLONE_TIMEOUT_SECS="${FETCH_OR_BUILD_TOOL_GIT_CLONE_TIMEOUT_SECS:-300}"

tool="${1:?tool required}"
target="${2:?target required}"
build_tool="${3:?build-tool required (zigbuild|xwin|empty for native)}"
exe_suffix="${4:-}"
dest_dir="${5:?dest-dir required}"

# Resolve the pinned tool version out of the source tree.
case "$tool" in
  crgx)
    version=$(sed -n 's/.*MANAGED_CRGX_VERSION: &str = "\(.*\)";/\1/p' \
      crates/soldr-cli/src/fetch/mod.rs | head -n1)
    upstream_repo="https://github.com/yfedoseev/crgx.git"
    env_var="CRGX_SOURCE_COMMIT"
    cargo_extra_args=""
    ;;
  cargo-chef)
    version=$(sed -n 's/.*CARGO_CHEF_PINNED_VERSION: &str = "\(.*\)";/\1/p' \
      crates/soldr-cli/src/fetch/known_tools.rs | head -n1)
    upstream_repo="https://github.com/LukeMathWalker/cargo-chef.git"
    env_var="CARGO_CHEF_SOURCE_COMMIT"
    cargo_extra_args="--bin cargo-chef"
    ;;
  *)
    echo "fetch_or_build_tool.sh: unknown tool '$tool'" >&2
    exit 2
    ;;
esac

if [ -z "$version" ]; then
  echo "fetch_or_build_tool.sh: failed to resolve version for $tool" >&2
  exit 2
fi

mkdir -p "$dest_dir"

# Map rust target triple -> toolchain asset query CLI flags.
case "$target" in
  x86_64-pc-windows-msvc)    q="--platform windows --arch x86 --extra msvc" ;;
  aarch64-pc-windows-msvc)   q="--platform windows --arch arm --extra msvc" ;;
  x86_64-unknown-linux-gnu)  q="--platform linux   --arch x86 --extra gnu"  ;;
  aarch64-unknown-linux-gnu) q="--platform linux   --arch arm --extra gnu"  ;;
  x86_64-unknown-linux-musl) q="--platform linux   --arch x86 --extra musl" ;;
  aarch64-unknown-linux-musl) q="--platform linux  --arch arm --extra musl" ;;
  x86_64-apple-darwin)       q="--platform mac     --arch x86" ;;
  aarch64-apple-darwin)      q="--platform mac     --arch arm" ;;
  *)
    echo "fetch_or_build_tool.sh: no catalogue mapping for target $target" >&2
    exit 2
    ;;
esac

asset_url=""
# toolchain_asset_query.py exits non-zero on a miss; capture stdout if it
# succeeds. The version we ask for is the soldr-source-tree-pinned
# version so we can never end up with a catalogue hit for some other
# tag.
if asset_url=$(python3 .github/scripts/toolchain_asset_query.py \
    $q --version "$version" "$tool" 2>/dev/null); then
  echo "catalogue hit: $tool $version $target -> $asset_url"
fi

if [ -n "$asset_url" ]; then
  # Fast path: download + extract the prebuilt and we're done.
  asset_filename=$(basename "$asset_url")
  if ! curl --fail --location \
      --connect-timeout "$CURL_CONNECT_TIMEOUT_SECS" \
      --max-time "$CURL_MAX_TIME_SECS" \
      --retry 6 --retry-delay 5 --retry-all-errors \
      --output "/tmp/${asset_filename}" "$asset_url"; then
    echo "catalogue URL fetch failed; falling back to source build" >&2
    asset_url=""
  else
    extract_tmp=$(mktemp -d)
    case "$asset_filename" in
      *.tar.zst|*.tar.zstd)
        tar --use-compress-program='zstd -d' \
            -xf "/tmp/${asset_filename}" -C "$extract_tmp"
        ;;
      *.tar.gz)
        tar -xzf "/tmp/${asset_filename}" -C "$extract_tmp"
        ;;
      *.zip)
        unzip -oq "/tmp/${asset_filename}" -d "$extract_tmp"
        ;;
      *)
        tar -xaf "/tmp/${asset_filename}" -C "$extract_tmp" || {
          echo "unknown archive format: $asset_filename; falling back" >&2
          asset_url=""
        }
        ;;
    esac
    if [ -n "$asset_url" ]; then
      binary_src=$(find "$extract_tmp" -type f -name "${tool}${exe_suffix}" -print -quit)
      if [ -z "$binary_src" ]; then
        echo "extracted archive missing ${tool}${exe_suffix}; falling back" >&2
        find "$extract_tmp" -maxdepth 3 -print >&2 || true
        asset_url=""
      else
        cp "$binary_src" "${dest_dir}/${tool}${exe_suffix}"
        chmod +x "${dest_dir}/${tool}${exe_suffix}" || true
        rm -rf "$extract_tmp"
        if [ -n "${GITHUB_ENV:-}" ]; then
          echo "${env_var}=prebuilt" >> "$GITHUB_ENV"
        fi
        echo "fetched prebuilt $tool $version for $target into $dest_dir"
        exit 0
      fi
    fi
  fi
fi

# Fallback: source build (preserves the previous behavior verbatim).
echo "catalogue miss for $tool $version $target; source-building"
work_dir="${RUNNER_TEMP:-/tmp}/${tool}-build"
rm -rf "$work_dir"
timeout "$GIT_CLONE_TIMEOUT_SECS" \
  git clone --depth 1 --branch "v${version}" "$upstream_repo" "$work_dir"
if [ -n "${GITHUB_ENV:-}" ]; then
  echo "${env_var}=$(git -C "$work_dir" rev-parse HEAD)" >> "$GITHUB_ENV"
fi
.github/scripts/print_build_banner.sh \
    "${tool} v${version}" \
    release \
    "$target" \
    "soldr cargo ${build_tool:-build} build" \
    "$work_dir/Cargo.toml"

case "$build_tool" in
  xwin)
    # shellcheck disable=SC2086
    soldr cargo xwin build \
      --release \
      --manifest-path "$work_dir/Cargo.toml" \
      --target "$target" \
      --locked $cargo_extra_args
    ;;
  zigbuild)
    # shellcheck disable=SC2086
    soldr cargo zigbuild \
      --release \
      --manifest-path "$work_dir/Cargo.toml" \
      --target "$target" \
      --locked $cargo_extra_args
    ;;
  ""|"native")
    # shellcheck disable=SC2086
    soldr cargo build \
      --release \
      --manifest-path "$work_dir/Cargo.toml" \
      --target "$target" \
      --locked $cargo_extra_args
    ;;
  *)
    echo "fetch_or_build_tool.sh: unknown build-tool '$build_tool'" >&2
    exit 2
    ;;
esac

built="$work_dir/target/${target}/release/${tool}${exe_suffix}"
if [ ! -f "$built" ]; then
  echo "source build completed but binary missing at $built" >&2
  ls -la "$work_dir/target/${target}/release/" >&2 || true
  exit 1
fi
cp "$built" "${dest_dir}/${tool}${exe_suffix}"
chmod +x "${dest_dir}/${tool}${exe_suffix}" || true
echo "source-built $tool $version for $target into $dest_dir"
