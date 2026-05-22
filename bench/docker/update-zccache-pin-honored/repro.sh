#!/usr/bin/env bash
# Reproduction script for zackees/soldr#424. Runs inside the harness
# container (see Dockerfile in this directory). Exits 0 if soldr honored
# the path pin, 1 if the bug from #424 reproduces, 2 on setup failure.
#
# The diagnostic this script grep's for was added by PR #421 specifically
# to make the pin-vs-managed routing observable:
#
#   soldr: zccache source: pinned (<dir>) version=...
#   soldr: zccache source: managed (<dir>) version=... (downloaded|cached)
#
# If a pin is registered but the routing logic ignores it (the bug),
# we'll see `managed` instead of `pinned`.

set -euo pipefail

PINNED_DIR="/pinned-bin"
MARKER="this-is-the-pinned-binary-$(cat /proc/sys/kernel/random/uuid)"

step() { printf '\n=== %s ===\n' "$*"; }
fail() {
    printf '\n!!! REPRO FAILED: %s\n' "$*" >&2
    printf '\n--- diagnostic dump ---\n' >&2
    printf 'pin status:\n'           >&2
    soldr update-zccache --status --json 2>&1 | jq . >&2 || true
    printf '\n~/.soldr/bin layout:\n' >&2
    ls -la "$HOME/.soldr/bin"        2>&1 >&2 || true
    for log in /tmp/cargo-build.stderr /tmp/cargo-build-soldr-cache-dir.stderr; do
        if [ -f "$log" ]; then
            printf '\n%s (last 80 lines):\n' "$log" >&2
            tail -n 80 "$log" >&2 || true
        else
            printf '\n%s: missing\n' "$log" >&2
        fi
    done
    exit 1
}
setup_fail() {
    printf '\n!!! SETUP FAILED: %s\n' "$*" >&2
    exit 2
}

step 'Stage fake zccache binaries at /pinned-bin'
mkdir -p "$PINNED_DIR"
# Three shell-script stubs that each carry the marker (so the SHA256
# soldr records is unique to this run and traceable in --status output).
# They speak enough of the zccache protocol that `update-zccache` and a
# subsequent build attempt won't immediately bail. The fake daemon
# doesn't actually compile anything — that's fine, we only care about
# which binary soldr decided to spawn, and that's emitted to stderr
# before any compile work.
for name in zccache zccache-daemon zccache-fp; do
    cat > "$PINNED_DIR/$name" <<EOF
#!/bin/sh
# marker: $MARKER
case "\$1" in
    --version) echo "zccache 1.8.1 (PINNED-FAKE)"; exit 0;;
    start|stop|clear) exit 0;;
    status) echo "hits=0"; exit 0;;
    session-start)
        # update-zccache and prepare_zccache_build both invoke this. The
        # 4th and 6th args are status/stats file paths that soldr expects
        # to exist (see fake_zccache_script in crates/soldr-cli/tests/common/mod.rs).
        [ -n "\${4:-}" ] && : > "\$4"
        [ -n "\${6:-}" ] && : > "\$6"
        echo '{"session_id":"pinned-fake-session"}'
        exit 0
        ;;
    session-end)
        echo '{"status":"ok","session_id":"pinned-fake-session","duration_ms":0,"compilations":0,"hits":0,"misses":0,"non_cacheable":0,"errors":0,"time_saved_ms":0,"unique_sources":0,"bytes_read":0,"bytes_written":0,"hit_rate":0}'
        exit 0
        ;;
    rust-plan)
        echo '{"operation":"'"\$2"'","compatibility":{"status":"ok","errors":[]}}'
        exit 0
        ;;
    flush)
        if [ "\${2:-}" = "--json" ]; then
            echo '{"status":"ok","bytes_written":0,"duration_ms":0}'
        else
            echo "flushed"
        fi
        exit 0
        ;;
esac
# When invoked as RUSTC_WRAPPER, argv is "<rustc> <rustc-args...>". Just
# delegate to rustc so the compile produces something on disk; the
# session stats we'd return are still fine because we already lied
# about them above.
"\$@"
EOF
    chmod +x "$PINNED_DIR/$name"
done

ls -la "$PINNED_DIR"

step 'soldr update-zccache /pinned-bin --json'
PIN_REGISTER_JSON=$(soldr update-zccache "$PINNED_DIR" --json 2>&1) \
    || setup_fail "update-zccache failed:\n$PIN_REGISTER_JSON"
echo "$PIN_REGISTER_JSON" | jq . || echo "$PIN_REGISTER_JSON"

step 'soldr update-zccache --status --json'
STATUS_JSON=$(soldr update-zccache --status --json) \
    || setup_fail "update-zccache --status failed"
echo "$STATUS_JSON" | jq .

source_kind=$(echo "$STATUS_JSON" | jq -r '.pinned.source_kind // empty')
source_value=$(echo "$STATUS_JSON" | jq -r '.pinned.source_value // empty')
if [ "$source_kind" != "path" ] || [ "$source_value" != "$PINNED_DIR" ]; then
    setup_fail "pin status did not register a path pin: source_kind='$source_kind' source_value='$source_value'"
fi
echo "pin registered: source_kind=$source_kind source_value=$source_value"

step 'Set up minimal Cargo project at /work'
mkdir -p src
cat > Cargo.toml <<'EOF'
[package]
name = "pin-repro"
version = "0.0.0"
edition = "2021"
EOF
echo 'fn main() {}' > src/main.rs

step 'soldr cargo build (captures the source-diagnostic line)'
# We don't care whether the build succeeds. The diagnostic is printed
# BEFORE any compile work, so the bug is observable even if the fake
# daemon later fails. `|| true` keeps the script alive past a non-zero
# build exit.
soldr cargo build 2>/tmp/cargo-build.stderr || true

step '~/.soldr/bin layout after the build'
# Always dump this — it's the cheapest way to see whether soldr
# downloaded a managed binary alongside the pinned one (the smoking
# gun pattern called out in #424's debugging notes).
ls -la "$HOME/.soldr/bin" 2>/dev/null || echo "(no $HOME/.soldr/bin)"

step 'Inspect soldr stderr for the zccache source diagnostic'
SOURCE_LINE=$(grep -m1 -E '^soldr: zccache source: (pinned|managed|local|system|unrecognized)' \
              /tmp/cargo-build.stderr || true)
if [ -z "$SOURCE_LINE" ]; then
    fail "soldr did not print the 'soldr: zccache source:' diagnostic — \
the build either failed before reaching the wrapper-prep step or the \
diagnostic was removed. Inspect /tmp/cargo-build.stderr inside the container."
fi
echo "$SOURCE_LINE"

# Cross-check: even when the diagnostic claims `pinned`, a `zccache-<ver>/`
# sibling dir means soldr ALSO downloaded a managed binary — which is
# benign (the pin still wins resolution) but worth flagging because it's
# the artifact pattern #424's debugging notes asked to look for.
MANAGED_DIRS=$(ls -d "$HOME/.soldr/bin"/zccache-[0-9]* 2>/dev/null || true)
if [ -n "$MANAGED_DIRS" ]; then
    printf '\nnote: a managed zccache install also exists alongside the pin:\n'
    echo "$MANAGED_DIRS"
fi

# === Default cache dir verdict ==========================================
step 'Default cache dir verdict'
case "$SOURCE_LINE" in
    "soldr: zccache source: pinned"*)
        echo "PASS (default cache dir): pin honored."
        ;;
    "soldr: zccache source: managed"*)
        fail "BUG #424 REPRODUCED (default cache dir): pin registered (source_kind=path, \
source_value=$PINNED_DIR) but soldr cargo build spawned the MANAGED \
zccache anyway. Diagnostic line: $SOURCE_LINE"
        ;;
    *)
        fail "unexpected zccache source classification (default cache dir) — got: $SOURCE_LINE"
        ;;
esac

# === SOLDR_CACHE_DIR variant (#426) ====================================
# Same setup, different cache root. Before #426's fix, the pin lookup
# under a custom SOLDR_CACHE_DIR resolved against `<SOLDR_CACHE_DIR>/bin/`
# (which is empty), so soldr silently bypassed the pin and downloaded
# the managed v1.8.1. The fix anchors pinned_bin at $HOME/.soldr/bin/
# so the pin survives any SOLDR_CACHE_DIR override.
step 'Re-run with SOLDR_CACHE_DIR=/work-cache — pin must STILL be honored (#426)'
mkdir -p /work-cache
# Wipe target/ so cargo actually re-invokes the wrapper. The trampoline
# fast-path (#346 / #354) skips the wrapper-prep step entirely when the
# previous build's artifacts are still up-to-date — which means the
# `soldr: zccache source:` diagnostic never fires, and we can't tell
# whether the pin was honored or not. A fresh `target/` forces a real
# compile.
rm -rf target
SOLDR_CACHE_DIR=/work-cache soldr cargo build 2>/tmp/cargo-build-soldr-cache-dir.stderr || true

ls -la /work-cache/bin 2>/dev/null || echo "(no /work-cache/bin — expected when pin wins)"

SOURCE_LINE_426=$(grep -m1 -E '^soldr: zccache source: (pinned|managed|local|system|unrecognized)' \
                  /tmp/cargo-build-soldr-cache-dir.stderr || true)
if [ -z "$SOURCE_LINE_426" ]; then
    fail "#426 variant: soldr did not print the 'soldr: zccache source:' \
diagnostic under SOLDR_CACHE_DIR=/work-cache — inspect \
/tmp/cargo-build-soldr-cache-dir.stderr inside the container."
fi
echo "$SOURCE_LINE_426"

step '#426 verdict'
case "$SOURCE_LINE_426" in
    "soldr: zccache source: pinned"*)
        echo "PASS (#426): pin survives SOLDR_CACHE_DIR override."
        exit 0
        ;;
    "soldr: zccache source: managed"*)
        fail "BUG #426 REPRODUCED: pin registered against the default cache \
root but \`SOLDR_CACHE_DIR=/work-cache soldr cargo build\` bypassed it \
and spawned the managed zccache anyway. Diagnostic line: $SOURCE_LINE_426"
        ;;
    *)
        fail "unexpected zccache source classification (#426 variant) — got: $SOURCE_LINE_426"
        ;;
esac
