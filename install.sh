#!/usr/bin/env bash
set -euo pipefail

REPO="${SOLDR_REPO:-zackees/soldr}"
INSTALL_DIR="${SOLDR_INSTALL_DIR:-$HOME/.local/bin}"
VERSION=""

usage() {
  cat <<'EOF'
Install soldr from GitHub Releases.

Usage:
  install.sh [--version <semver-or-tag>] [--bin-dir <path>]

Environment:
  SOLDR_REPO         Override the GitHub repository (default: zackees/soldr)
  SOLDR_INSTALL_DIR  Override the installation directory (default: ~/.local/bin)
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version)
      VERSION="${2:-}"
      shift 2
      ;;
    --bin-dir)
      INSTALL_DIR="${2:-}"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

need_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required command: $1" >&2
    exit 1
  fi
}

need_cmd curl

PYTHON_BIN=""
if command -v python3 >/dev/null 2>&1; then
  PYTHON_BIN="python3"
elif command -v python >/dev/null 2>&1; then
  PYTHON_BIN="python"
else
  echo "missing required command: python3 or python" >&2
  exit 1
fi

detect_target() {
  local uname_s uname_m os arch
  uname_s="$(uname -s)"
  uname_m="$(uname -m)"

  case "$uname_m" in
    x86_64|amd64) arch="x86_64" ;;
    arm64|aarch64) arch="aarch64" ;;
    *)
      echo "unsupported architecture: $uname_m" >&2
      exit 1
      ;;
  esac

  case "$uname_s" in
    Linux) os="unknown-linux-gnu" ;;
    Darwin) os="apple-darwin" ;;
    MINGW*|MSYS*|CYGWIN*) os="pc-windows-msvc" ;;
    *)
      echo "unsupported operating system: $uname_s" >&2
      exit 1
      ;;
  esac

  printf '%s-%s\n' "$arch" "$os"
}

fetch_release_json() {
  local url
  if [[ -n "$VERSION" ]]; then
    local tag="$VERSION"
    if [[ "$tag" != v* ]]; then
      tag="v$tag"
    fi
    url="https://api.github.com/repos/${REPO}/releases/tags/${tag}"
  else
    url="https://api.github.com/repos/${REPO}/releases/latest"
  fi

  curl -fsSL \
    -H "Accept: application/vnd.github+json" \
    -H "X-GitHub-Api-Version: 2022-11-28" \
    "$url"
}

TARGET="$(detect_target)"
ARCHIVE_EXT="tar.gz"
BINARY_NAME="soldr"

if [[ "$TARGET" == *windows-msvc ]]; then
  ARCHIVE_EXT="zip"
  BINARY_NAME="soldr.exe"
  need_cmd unzip
else
  need_cmd tar
fi

if ! RELEASE_JSON="$(fetch_release_json)"; then
  echo "failed to query GitHub Releases; falling back to pip install" >&2
  if [[ -n "$VERSION" ]]; then
    "$PYTHON_BIN" -m pip install --user "soldr==${VERSION}"
  else
    "$PYTHON_BIN" -m pip install --user soldr
  fi
  exit 0
fi

readarray -t RELEASE_INFO < <(
  RELEASE_JSON="$RELEASE_JSON" TARGET="$TARGET" ARCHIVE_EXT="$ARCHIVE_EXT" "$PYTHON_BIN" - <<'PY'
import json
import os
import sys

body = json.loads(os.environ["RELEASE_JSON"])
target = os.environ["TARGET"]
archive_ext = os.environ["ARCHIVE_EXT"]

assets = body.get("assets") or []
match = next(
    (
        asset
        for asset in assets
        if target in asset.get("name", "") and asset.get("name", "").endswith(archive_ext)
    ),
    None,
)

if match is None:
    sys.exit(1)

print(body["tag_name"])
print(match["name"])
print(match["browser_download_url"])
PY
)

if [[ "${#RELEASE_INFO[@]}" -ne 3 ]]; then
  echo "no release asset found for target ${TARGET}; falling back to pip install" >&2
  if [[ -n "$VERSION" ]]; then
    "$PYTHON_BIN" -m pip install --user "soldr==${VERSION}"
  else
    "$PYTHON_BIN" -m pip install --user soldr
  fi
  exit 0
fi

TAG_NAME="${RELEASE_INFO[0]}"
ASSET_NAME="${RELEASE_INFO[1]}"
DOWNLOAD_URL="${RELEASE_INFO[2]}"

TMP_DIR="$(mktemp -d)"
cleanup() {
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT

ASSET_PATH="${TMP_DIR}/${ASSET_NAME}"
EXTRACT_DIR="${TMP_DIR}/extract"
mkdir -p "$EXTRACT_DIR" "$INSTALL_DIR"

curl -fsSL "$DOWNLOAD_URL" -o "$ASSET_PATH"

if [[ "$ARCHIVE_EXT" == "zip" ]]; then
  unzip -q "$ASSET_PATH" -d "$EXTRACT_DIR"
else
  tar -xzf "$ASSET_PATH" -C "$EXTRACT_DIR"
fi

SOURCE_PATH="${EXTRACT_DIR}/${BINARY_NAME}"
if [[ ! -f "$SOURCE_PATH" ]]; then
  echo "downloaded archive did not contain ${BINARY_NAME}" >&2
  exit 1
fi

DEST_PATH="${INSTALL_DIR}/${BINARY_NAME}"
install -m 755 "$SOURCE_PATH" "$DEST_PATH"

cat <<EOF
installed ${BINARY_NAME} from ${TAG_NAME} to ${DEST_PATH}

ensure ${INSTALL_DIR} is on PATH, then run:
  ${BINARY_NAME} version
EOF
