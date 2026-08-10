#!/bin/sh
set -eu

RP_INSTALL_MODE="${RP_INSTALL_MODE:-user}"
RP_INSTALL_REPO="${RP_INSTALL_REPO:-zackees/running-process}"
RP_INSTALL_BASE_URL="${RP_INSTALL_BASE_URL:-}"
RP_INSTALL_VERSION="${RP_INSTALL_VERSION:-latest}"
RP_NO_MODIFY_PATH="${RP_NO_MODIFY_PATH:-0}"

usage() {
    cat <<'EOF'
Usage: install.sh [--user|--global] [--bin-dir PATH] [--version VERSION]

Installs the standalone `runpm` and `running-process-daemon` binaries.

Environment:
  RP_INSTALL_MODE      user or global
  RP_INSTALL_DIR       explicit install directory
  RP_INSTALL_VERSION   latest or a specific version/tag
  RP_INSTALL_REPO      GitHub repo owner/name
  RP_INSTALL_BASE_URL  Override release base URL (for testing/mirrors)
  RP_NO_MODIFY_PATH    1 to skip shell profile updates
EOF
}

log() {
    printf '[running-process-install] %s\n' "$*"
}

die() {
    printf '[running-process-install] ERROR: %s\n' "$*" >&2
    exit 1
}

need_cmd() {
    command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

append_path_line() {
    profile="$1"
    line="$2"
    [ -f "$profile" ] || : >"$profile"
    grep -F "$line" "$profile" >/dev/null 2>&1 || printf '\n%s\n' "$line" >>"$profile"
}

modify_path() {
    install_dir="$1"
    case ":${PATH:-}:" in
        *:"$install_dir":*) return 0 ;;
    esac
    if [ "$RP_NO_MODIFY_PATH" = "1" ]; then
        return 0
    fi
    export_line="export PATH=\"$install_dir:\$PATH\""
    append_path_line "$HOME/.profile" "$export_line"
    if [ -n "${SHELL:-}" ] && [ "$(basename "$SHELL")" = "zsh" ]; then
        append_path_line "$HOME/.zprofile" "$export_line"
    fi
    log "Added $install_dir to shell startup PATH configuration."
}

normalize_arch() {
    case "$1" in
        x86_64|amd64) printf 'x86_64' ;;
        arm64|aarch64) printf 'aarch64' ;;
        *) die "unsupported architecture: $1" ;;
    esac
}

detect_target() {
    os="$(uname -s)"
    arch="$(normalize_arch "$(uname -m)")"
    case "$os" in
        Linux) printf '%s-unknown-linux-gnu' "$arch" ;;
        Darwin) printf '%s-apple-darwin' "$arch" ;;
        *) die "unsupported operating system: $os" ;;
    esac
}

download() {
    url="$1"
    dest="$2"
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$url" -o "$dest"
    elif command -v wget >/dev/null 2>&1; then
        wget -qO "$dest" "$url"
    else
        die "either curl or wget is required"
    fi
}

resolve_latest_tag() {
    api_url="https://api.github.com/repos/$RP_INSTALL_REPO/releases/latest"
    if command -v curl >/dev/null 2>&1; then
        body="$(curl -fsSL "$api_url")"
    elif command -v wget >/dev/null 2>&1; then
        body="$(wget -qO- "$api_url")"
    else
        die "either curl or wget is required to resolve latest version"
    fi
    tag="$(printf '%s' "$body" \
        | tr -d '\n' \
        | sed -n 's/.*"tag_name":[[:space:]]*"\([^"]*\)".*/\1/p')"
    [ -n "$tag" ] || die "could not parse latest release tag from $api_url"
    printf '%s' "$tag"
}

normalize_version() {
    version="$1"
    case "$version" in
        v*) printf '%s' "${version#v}" ;;
        *) printf '%s' "$version" ;;
    esac
}

asset_url() {
    tag="$1"
    asset="$2"
    if [ -n "$RP_INSTALL_BASE_URL" ]; then
        base="$RP_INSTALL_BASE_URL"
    else
        base="https://github.com/$RP_INSTALL_REPO/releases"
    fi
    printf '%s/download/%s/%s' "$base" "$tag" "$asset"
}

extract_archive() {
    archive="$1"
    dest="$2"
    mkdir -p "$dest"
    tar -xzf "$archive" -C "$dest"
}

main() {
    install_dir="${RP_INSTALL_DIR:-}"
    version="$RP_INSTALL_VERSION"

    while [ "$#" -gt 0 ]; do
        case "$1" in
            --user) RP_INSTALL_MODE="user" ;;
            --global) RP_INSTALL_MODE="global" ;;
            --bin-dir)
                shift
                [ "$#" -gt 0 ] || die "--bin-dir requires a value"
                install_dir="$1"
                ;;
            --version)
                shift
                [ "$#" -gt 0 ] || die "--version requires a value"
                version="$1"
                ;;
            --help|-h)
                usage
                exit 0
                ;;
            *)
                die "unknown argument: $1"
                ;;
        esac
        shift
    done

    need_cmd tar
    need_cmd mktemp

    if [ -z "$install_dir" ]; then
        if [ "$RP_INSTALL_MODE" = "global" ]; then
            install_dir="/usr/local/bin"
        else
            install_dir="$HOME/.local/bin"
        fi
    fi

    if [ "$version" = "latest" ]; then
        tag="$(resolve_latest_tag)"
    else
        case "$version" in
            v*) tag="$version" ;;
            *) tag="$version" ;;
        esac
    fi
    ver="$(normalize_version "$tag")"

    target="$(detect_target)"
    asset="running-process-${ver}-${target}.tar.gz"
    url="$(asset_url "$tag" "$asset")"

    tmpdir="$(mktemp -d 2>/dev/null || mktemp -d -t running-process-install)"
    trap 'rm -rf "$tmpdir"' EXIT INT TERM

    archive="$tmpdir/$asset"
    log "Downloading $url"
    download "$url" "$archive"
    extract_archive "$archive" "$tmpdir"

    archive_root="$tmpdir/running-process-${ver}-${target}"
    [ -d "$archive_root" ] || die "archive layout was not recognized"

    mkdir -p "$install_dir"
    cp "$archive_root"/runpm "$install_dir"/
    cp "$archive_root"/running-process-daemon "$install_dir"/
    chmod 755 "$install_dir"/runpm "$install_dir"/running-process-daemon 2>/dev/null || true

    if [ "$RP_INSTALL_MODE" = "user" ]; then
        modify_path "$install_dir"
    fi

    log "Installed runpm + running-process-daemon to $install_dir"
    if ! command -v runpm >/dev/null 2>&1; then
        log "Open a new shell or export PATH=\"$install_dir:\$PATH\" before running runpm."
    fi
}

main "$@"
