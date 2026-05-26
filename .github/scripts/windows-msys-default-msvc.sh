#!/usr/bin/env bash
set -euo pipefail

workspace_unix="$(cygpath -u "$GITHUB_WORKSPACE")"
fixture="$workspace_unix/crates/soldr-cli/tests/fixtures/windows-msvc-default"
fake_tools="$(mktemp -d)"
target_dir_win="$RUNNER_TEMP\\soldr-msys-default-msvc"
target_dir_unix="$(cygpath -u "$target_dir_win")"
cache_dir_win="$RUNNER_TEMP\\soldr-msys-default-msvc-cache"
soldr_exe="${SOLDR_EXE:-$workspace_unix/target/debug/soldr.exe}"

if [[ -n "${SOLDR_EXE:-}" ]]; then
  soldr_exe="$(cygpath -u "$SOLDR_EXE")"
fi

mkdir -p "$target_dir_unix"

cat >"$fake_tools/cargo.cmd" <<'EOF'
@echo off
echo fake cargo should not be used 1>&2
exit /b 1
EOF

cat >"$fake_tools/rustc.cmd" <<'EOF'
@echo off
echo fake rustc should not be used 1>&2
exit /b 1
EOF

export PATH="$fake_tools:$PATH"
export CARGO_TARGET_DIR="$target_dir_win"
export SOLDR_CACHE_DIR="$cache_dir_win"
export ZCCACHE_CACHE_DIR="$cache_dir_win\\cache\\zccache"
unset SOLDR_BUILD_CACHE_MODE
unset SOLDR_TARGET_CACHE_MODE
unset SOLDR_TARGET_CACHE_DIR
unset SOLDR_TARGET_CACHE_BUNDLE_DIR
unset SOLDR_TARGET_CACHE_BACKEND
unset SOLDR_TARGET_CACHE_PROFILE
unset SOLDR_LINKER
unset RUSTC_WRAPPER
unset RUSTC_WORKSPACE_WRAPPER

cd "$fixture"
"$soldr_exe" --no-cache cargo build --locked

artifact="$target_dir_unix/x86_64-pc-windows-msvc/debug/windows-msvc-default.exe"
if [[ ! -f "$artifact" ]]; then
  echo "expected MSVC artifact at $artifact" >&2
  echo "observed fixture artifacts:" >&2
  find "$target_dir_unix" -maxdepth 5 -type f -name 'windows-msvc-default*.exe' -print >&2 || true
  exit 1
fi
