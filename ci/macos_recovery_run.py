#!/usr/bin/env python3
"""Per-PR macOS x86_64 execution lane, hosted in a Recovery guest (soldr#3076/#3078).

`e2e-macos-x64` in `ci.yml` cross-builds `x86_64-apple-darwin` on Linux (via
`_ci-cross-build-linux.yml`) and replays the packaged nextest archive inside a
`zackees/docker-mac-x64` Recovery guest -- a real x86_64 macOS **Recovery**
guest, no baked image, no secret, no ssh (soldr#3076). Recovery boots fresh
per action invocation, runs exactly one script (typed into a GUI Terminal and
fetched back over HTTP), and `/tmp` is a ramdisk that nothing survives past
that one boot. There is no toolchain to provision ahead of time and no room
for the decompressed nextest archive on the ramdisk -- but Recovery *can*
format and mount the action's blank 64 GB qcow2 disk, which gives the guest
script a real, persistent-for-the-boot volume to extract onto and provision a
managed rustup toolchain under (soldr#3078).

So this lane replays the same positively-owned host-sensitive nextest
partition every other `target-run` lane replays (`_ci-target-run.yml`'s "Run
owned pre-built native tests" step), just packed into one guest script
instead of a dozen workflow steps, because Recovery has no per-command exec:

    emit-guest-script --output PATH
        Write the bash-3.2/POSIX-sh-compatible script the guest runs. Pure
        function of nothing but the module constants, so it needs no
        arguments beyond where to write it.

    verify-collected --collected DIR --guest-exit-code CODE [--manifest ...]
        Read the guest's collected results (the action's `collect` tarball,
        already extracted by the workflow) plus its `exit-code` output, and
        fail with a named diagnostic per check. When `--manifest`,
        `--repo-root`, and `--target` are given, also runs the ownership
        inventory validation and the coverage summary check against the
        `all-list.json` / `list.json` / `junit.xml` the guest produced --
        the same two checks the native `target-run` path runs, just deferred
        to after collection because Recovery has no Linux-side inventory to
        validate the filter against before it boots (see
        `target_run_ownership.build_filter_expression`).

Guest facts this script is built around (see the PR description for the
source): Ventura Recovery, root, one script per boot; `sh`/`bash 3.2`,
`curl`, `tar` (bsdtar, no zstd); the action's blank qcow2 disk can be
formatted with `diskutil eraseDisk APFS Work <disk>` and mounted at
`/Volumes/Work`, falling back to `/tmp/work` (ramdisk) if that fails;
`cargo-nextest` decodes `.tar.zst` itself, so only the workspace and fixtures
tarballs need to be plain, uncompressed `.tar` for bsdtar to extract.
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from pathlib import Path

GUEST_HTTP_BASE = "http://10.0.2.2:8000"
RESULTS_FILE = "summary.txt"
# The action attaches a blank 64 GiB qcow2 as the guest hard disk (docker-mac-x64
# action.yml: `qemu-img create -f qcow2 ... 64G`). `diskutil list` never prints
# the media model, so the disk is picked by size: the only whole disk at or
# above this floor. The Base System image (~2 GB) and OpenCore (~1 GB) sit far
# below it.
MIN_SCRATCH_DISK_BYTES = 60 * 1000 * 1000 * 1000

# soldr#3076/#3078: every stage this lane's guest script runs, in the order
# it runs them. The first four are the original binary-only smoke (kept as
# the first stage); everything after `storage` is the archive replay added
# by soldr#3078. `verify_collected` treats this generically -- it only needs
# every name here to appear as a `pass` line in the guest's `summary.txt` --
# so the guest script is the single place that has to stay in sync with it.
CHECKS = (
    "arch",
    "fetch_soldr",
    "version",
    "help",
    "storage",
    "fetch_soldr-daemon",
    "fetch_cargo-nextest",
    "fetch_tests.tar.zst",
    "fetch_fixtures.tar",
    "fetch_workspace.tar",
    "fetch_filter.txt",
    "fetch_nextest-version.txt",
    "extract_workspace",
    "runner_shim",
    "extract_fixtures",
    "toolchain_ensure",
    "toolchain_link",
    "nextest_version",
    "nextest_list_all",
    "nextest_list_selected",
    "nextest_run",
)

# Share-dir file names the Linux-side prep step must populate before the
# guest boots (soldr#3078). "soldr" is fetched by the first stage instead --
# it is needed before `storage` even runs, and there is no reason to fetch it
# twice into two locations for the same boot.
REPLAY_SHARE_FILES = (
    "soldr-daemon",
    "cargo-nextest",
    "tests.tar.zst",
    "fixtures.tar",
    "workspace.tar",
    "filter.txt",
    "nextest-version.txt",
)

# ~2 MiB: enough of `nextest run`'s tail to diagnose a failure without
# ballooning the diagnostics artifact (the collected directory is uploaded
# whole; the extracted nextest archive itself is never copied there).
NEXTEST_LOG_TAIL_BYTES = 2_000_000


def build_guest_script() -> str:
    """The bash-3.2/POSIX-sh script the Recovery guest runs.

    Never raises on a failed stage: every stage runs (unless a hard
    prerequisite it depends on already failed) and is recorded in
    `/tmp/results/summary.txt` as a `name=pass[:detail]` /
    `name=fail[:detail]` line via the `record` shell function, and the
    script's own exit code (0 only if every stage passed) is the second,
    coarser signal `verify-collected` checks against the action's
    `exit-code` output -- the same belt-and-suspenders
    `smoke_release_artifacts.build_release_guest_script` uses.

    Stages that depend on an earlier stage's success track that with a
    plain `..._OK` shell variable and record a `fail:skipped: ...`
    diagnostic instead of running when the prerequisite did not hold, so a
    single early failure (say, the tests archive never fetched) still
    produces a summary that explains every stage it caused to be skipped,
    rather than stopping the script cold.
    """
    return "\n".join(
        [
            "#!/bin/sh",
            "set +e",
            "mkdir -p /tmp/results",
            f"SUMMARY=/tmp/results/{RESULTS_FILE}",
            ': > "$SUMMARY"',
            "FAIL=0",
            "",
            "# record NAME STATUS [DETAIL] -- flat key=value line, one per stage.",
            "# DETAIL is sanitized to a single line and length-capped: a raw",
            "# newline in an error message would otherwise corrupt the flat",
            "# summary.txt format, which is parsed one line per stage.",
            "record() {",
            '  rname="$1"',
            '  rstatus="$2"',
            '  rdetail="$3"',
            '  if [ -n "$rdetail" ]; then',
            "    rdetail=$(printf '%s' \"$rdetail\" | tr '\\n\\r' '  ' | cut -c1-400)",
            '    printf \'%s=%s:%s\\n\' "$rname" "$rstatus" "$rdetail" >> "$SUMMARY"',
            "  else",
            '    printf \'%s=%s\\n\' "$rname" "$rstatus" >> "$SUMMARY"',
            "  fi",
            '  if [ "$rstatus" = fail ]; then',
            "    FAIL=1",
            "  fi",
            "}",
            "",
            'stage_start() { echo "[$1] start"; }',
            'stage_end() { echo "[$1] end"; }',
            "",
            "# fetch NAME DEST [x] -- GET http://10.0.2.2:8000/NAME into DEST,",
            "# chmod +x DEST when the third arg is 'x'. Records fetch_NAME.",
            "fetch() {",
            f'  if ! curl -fsS -o "$2" "{GUEST_HTTP_BASE}/$1"; then',
            f'    record "fetch_$1" fail "curl could not reach {GUEST_HTTP_BASE}/$1"',
            "    return 1",
            "  fi",
            '  if [ "$3" = x ]; then',
            '    chmod +x "$2"',
            "  fi",
            '  record "fetch_$1" pass',
            "  return 0",
            "}",
            "",
            *_stage_core_smoke(),
            *_stage_storage(),
            *_stage_fetch_replay_inputs(),
            *_stage_extract(),
            *_stage_toolchain(),
            *_stage_nextest_version(),
            *_stage_nextest_list_all(),
            *_stage_nextest_list_selected(),
            *_stage_nextest_run(),
            *_stage_collect_results(),
            'exit "$FAIL"',
            "",
        ]
    )


def _stage_core_smoke() -> list[str]:
    """arch / fetch_soldr / version / help -- the original soldr#3076 smoke.

    `/tmp/soldr` (ramdisk) rather than a path under the storage volume: this
    stage runs before `storage` even determines a work volume, and nothing
    later needs a second copy -- the ramdisk persists for the whole boot.
    """
    return [
        "stage_start core",
        "ARCH=$(uname -m)",
        'if [ "$ARCH" = x86_64 ]; then',
        '  record arch pass "$ARCH"',
        "else",
        '  record arch fail "unexpected uname -m $ARCH"',
        "fi",
        "",
        "fetch soldr /tmp/soldr x",
        "SOLDR_BIN=/tmp/soldr",
        "export SOLDR_BIN",
        "",
        "if [ -x /tmp/soldr ]; then",
        "  VOUT=$(/tmp/soldr --version 2>&1)",
        '  case "$VOUT" in',
        '    "soldr "*)',
        '      record version pass "$VOUT" ;;',
        "    *)",
        '      record version fail "$VOUT" ;;',
        "  esac",
        "",
        "  /tmp/soldr --help >/tmp/help.out 2>&1",
        "  HRC=$?",
        '  if [ "$HRC" -eq 0 ]; then',
        "    record help pass",
        "  else",
        '    record help fail "exit $HRC"',
        "  fi",
        "else",
        '  record version fail "soldr binary missing (fetch_soldr failed)"',
        '  record help fail "soldr binary missing (fetch_soldr failed)"',
        "fi",
        "stage_end core",
        "",
    ]


def _stage_storage() -> list[str]:
    """Format+mount the action's blank qcow2 disk, or fall back to /tmp/work.

    `diskutil list` does not print media model names, so every whole disk it
    lists is sized through `diskutil info` and the largest one at or above
    MIN_SCRATCH_DISK_BYTES is chosen; the Base System and OpenCore disks are
    both far smaller and can never be picked.
    """
    return [
        "stage_start storage",
        "diskutil list > /tmp/diskutil-list.txt 2>/tmp/diskutil-list.err",
        ": > /tmp/diskutil-info.txt",
        'DISK=""',
        "BEST_BYTES=0",
        "for dev in $(sed -n 's#^\\(/dev/disk[0-9]*\\).*#\\1#p' /tmp/diskutil-list.txt); do",
        '  info=$(diskutil info "$dev" 2>/dev/null)',
        "  printf '%s\\n---\\n' \"$info\" >> /tmp/diskutil-info.txt",
        "  printf '%s\\n' \"$info\" | grep -q 'Whole:.*Yes' || continue",
        "  bytes=$(printf '%s\\n' \"$info\" | sed -n 's/.*Disk Size:.*(\\([0-9][0-9]*\\) Bytes).*/\\1/p' | head -n1)",
        "  case \"$bytes\" in ''|*[!0-9]*) continue ;; esac",
        f'  if [ "$bytes" -ge {MIN_SCRATCH_DISK_BYTES} ] && [ "$bytes" -gt "$BEST_BYTES" ]; then',
        '    DISK="${dev#/dev/}"',
        '    BEST_BYTES="$bytes"',
        "  fi",
        "done",
        "",
        'WORK=""',
        'if [ -n "$DISK" ]; then',
        '  if diskutil eraseDisk APFS Work "/dev/$DISK" > /tmp/erase.log 2>&1 && [ -d /Volumes/Work ]; then',
        "    WORK=/Volumes/Work",
        "  fi",
        "fi",
        'if [ -z "$WORK" ]; then',
        "  WORK=/tmp/work",
        '  mkdir -p "$WORK"',
        "fi",
        "",
        "FREE_KB=$(df -k \"$WORK\" 2>/dev/null | awk 'NR==2{print $4}')",
        'case "$FREE_KB" in',
        "  ''|*[!0-9]*) FREE_GIB=unknown ;;",
        "  *) FREE_GIB=$((FREE_KB / 1048576)) ;;",
        "esac",
        'if [ "$WORK" = /Volumes/Work ]; then',
        '  record storage pass "disk=$DISK path=$WORK free_gib=$FREE_GIB"',
        "else",
        '  record storage pass "fallback path=$WORK free_gib=$FREE_GIB disk=${DISK:-none}"',
        "fi",
        "export WORK",
        'mkdir -p "$WORK/home" "$WORK/tmp"',
        'HOME="$WORK/home"',
        "# Recovery's default temp dirs (/tmp, /var/folders) are ~5 GiB tmpfs;",
        "# 13 of 19 failures on PR #3087's third run were ENOSPC from tests'",
        "# tempdirs. Point every temp_dir() consumer at the scratch disk.",
        'TMPDIR="$WORK/tmp"',
        "export HOME TMPDIR",
        "",
        "# Env every archived test expects (mirrors the native target-run",
        "# path's 'Resolve packaged target tools' step). CARGO_HOME/RUSTUP_HOME",
        "# are deliberately left unset -- 'soldr toolchain ensure' exports its",
        "# own managed homes.",
        'SOLDR_INTERNAL_DAEMON_EXE="$WORK/soldr-daemon"',
        'NEXTEST_BIN="$WORK/cargo-nextest"',
        'SOLDR_TEST_WORKSPACE_ROOT="$WORK/workspace"',
        'SOLDR_TEST_FIXTURES_DIR="$WORK/fixtures"',
        "SOLDR_USE_SYSTEM_CMAKE=1",
        "SOLDR_TARGET_WARN_FREE_GB=1",
        "SOLDR_TARGET_BLOCK_FREE_GB=1",
        "export SOLDR_INTERNAL_DAEMON_EXE NEXTEST_BIN SOLDR_TEST_WORKSPACE_ROOT",
        "export SOLDR_TEST_FIXTURES_DIR SOLDR_USE_SYSTEM_CMAKE",
        "export SOLDR_TARGET_WARN_FREE_GB SOLDR_TARGET_BLOCK_FREE_GB",
        "stage_end storage",
        "",
    ]


def _stage_fetch_replay_inputs() -> list[str]:
    return [
        "stage_start fetch_replay_inputs",
        'fetch soldr-daemon "$WORK/soldr-daemon" x',
        "FETCH_SOLDR_DAEMON_OK=$?",
        'fetch cargo-nextest "$WORK/cargo-nextest" x',
        "FETCH_CARGO_NEXTEST_OK=$?",
        'fetch tests.tar.zst "$WORK/tests.tar.zst"',
        "FETCH_TESTS_OK=$?",
        'fetch fixtures.tar "$WORK/fixtures.tar"',
        "FETCH_FIXTURES_TAR_OK=$?",
        'fetch workspace.tar "$WORK/workspace.tar"',
        "FETCH_WORKSPACE_TAR_OK=$?",
        'fetch filter.txt "$WORK/filter.txt"',
        "FETCH_FILTER_OK=$?",
        'fetch nextest-version.txt "$WORK/nextest-version.txt"',
        "FETCH_NEXTVER_OK=$?",
        "stage_end fetch_replay_inputs",
        "",
    ]


def _stage_extract() -> list[str]:
    return [
        "stage_start extract_workspace",
        'if [ "$FETCH_WORKSPACE_TAR_OK" -eq 0 ]; then',
        '  mkdir -p "$WORK/workspace"',
        '  if tar -xf "$WORK/workspace.tar" -C "$WORK/workspace"; then',
        "    record extract_workspace pass",
        "    EXTRACT_WORKSPACE_OK=0",
        "    # .config/nextest.toml wraps every unix test in",
        "    # .github/scripts/nextest_timeout_wrapper.py (SIGTERM thread dumps),",
        "    # resolved relative to the workspace root. Recovery has no python3,",
        "    # so every test died with `env: python3: No such file or directory`",
        "    # (PR #3087's first run). Swap the wrapper for a POSIX exec shim; the",
        "    # nextest slow-timeout still bounds each test, only the dump is lost.",
        '    printf \'#!/bin/sh\\nexec "$@"\\n\' > "$WORK/workspace/.github/scripts/nextest_timeout_wrapper.py"',
        '    chmod +x "$WORK/workspace/.github/scripts/nextest_timeout_wrapper.py"',
        '    record runner_shim pass "nextest_timeout_wrapper.py replaced by exec shim"',
        "  else",
        '    record extract_workspace fail "tar extraction of workspace.tar failed"',
        "    EXTRACT_WORKSPACE_OK=1",
        "  fi",
        "else",
        '  record extract_workspace fail "skipped: workspace.tar was not fetched"',
        "  EXTRACT_WORKSPACE_OK=1",
        "fi",
        "stage_end extract_workspace",
        "",
        "stage_start extract_fixtures",
        'if [ "$FETCH_FIXTURES_TAR_OK" -eq 0 ]; then',
        '  mkdir -p "$WORK/fixtures"',
        '  if tar -xf "$WORK/fixtures.tar" -C "$WORK/fixtures"; then',
        "    record extract_fixtures pass",
        "    EXTRACT_FIXTURES_OK=0",
        "  else",
        '    record extract_fixtures fail "tar extraction of fixtures.tar failed"',
        "    EXTRACT_FIXTURES_OK=1",
        "  fi",
        "else",
        '  record extract_fixtures fail "skipped: fixtures.tar was not fetched"',
        "  EXTRACT_FIXTURES_OK=1",
        "fi",
        "stage_end extract_fixtures",
        "",
    ]


def _stage_toolchain() -> list[str]:
    """`soldr toolchain ensure` + `toolchain link`, run from the extracted
    workspace root so `rust-toolchain.toml` resolves the same way it does on
    every other target-run lane.

    There is no Python in the guest to run `toolchain_ensure_channel.py`, so
    the resolved channel is pulled out of the ensure JSON with `sed` instead
    -- tolerant of the same rustup-preamble noise that script's docstring
    describes, since it only looks for a line naming `"channel"` rather than
    parsing the payload as a whole.
    """
    return [
        "stage_start toolchain",
        'if [ "$EXTRACT_WORKSPACE_OK" -eq 0 ] && cd "$WORK/workspace"; then',
        '  "$SOLDR_BIN" toolchain ensure --json > "$WORK/toolchain-ensure.json" 2>&1',
        "  TE_RC=$?",
        '  if [ "$TE_RC" -eq 0 ]; then',
        "    CHANNEL=$(sed -n "
        + '\'s/.*"channel"[[:space:]]*:[[:space:]]*"\\([^"]*\\)".*/\\1/p\''
        + ' "$WORK/toolchain-ensure.json" | head -n1)',
        '    if [ -n "$CHANNEL" ]; then',
        '      record toolchain_ensure pass "channel=$CHANNEL"',
        '      RUSTUP_TOOLCHAIN="$CHANNEL"',
        "      export RUSTUP_TOOLCHAIN",
        "      TOOLCHAIN_ENSURE_OK=0",
        "    else",
        '      record toolchain_ensure fail "ensure succeeded but JSON names no channel"',
        "      TOOLCHAIN_ENSURE_OK=1",
        "    fi",
        "  else",
        '    record toolchain_ensure fail "soldr toolchain ensure exited $TE_RC"',
        "    TOOLCHAIN_ENSURE_OK=1",
        "  fi",
        "else",
        '  record toolchain_ensure fail "skipped: workspace extraction did not succeed"',
        "  TOOLCHAIN_ENSURE_OK=1",
        "fi",
        "",
        'if [ "$TOOLCHAIN_ENSURE_OK" -eq 0 ]; then',
        '  "$SOLDR_BIN" toolchain link --shim-dir "$WORK/shims" --json > "$WORK/toolchain-link.json" 2>&1',
        "  TL_RC=$?",
        '  if [ "$TL_RC" -eq 0 ]; then',
        "    record toolchain_link pass",
        '    PATH="$WORK/shims:$PATH"',
        "    export PATH",
        "    TOOLCHAIN_LINK_OK=0",
        "  else",
        '    record toolchain_link fail "soldr toolchain link exited $TL_RC"',
        "    TOOLCHAIN_LINK_OK=1",
        "  fi",
        "else",
        '  record toolchain_link fail "skipped: toolchain_ensure did not succeed"',
        "  TOOLCHAIN_LINK_OK=1",
        "fi",
        "stage_end toolchain",
        "",
    ]


def _stage_nextest_version() -> list[str]:
    return [
        "stage_start nextest_version",
        'if [ "$FETCH_CARGO_NEXTEST_OK" -eq 0 ] && [ "$FETCH_NEXTVER_OK" -eq 0 ]; then',
        '  ACTUAL=$("$NEXTEST_BIN" nextest --version 2>&1 '
        + "| sed -n 's/.*[^0-9]\\([0-9][0-9]*\\.[0-9][0-9]*\\.[0-9][0-9]*\\).*/\\1/p' "
        + "| head -n1)",
        '  EXPECTED=$(cat "$WORK/nextest-version.txt")',
        '  if [ -n "$ACTUAL" ] && [ "$ACTUAL" = "$EXPECTED" ]; then',
        '    record nextest_version pass "$ACTUAL"',
        "    NEXTEST_VERSION_OK=0",
        "  else",
        '    record nextest_version fail "expected=$EXPECTED actual=${ACTUAL:-<none>}"',
        "    NEXTEST_VERSION_OK=1",
        "  fi",
        "else",
        '  record nextest_version fail "skipped: cargo-nextest or nextest-version.txt not fetched"',
        "  NEXTEST_VERSION_OK=1",
        "fi",
        "stage_end nextest_version",
        "",
    ]


def _stage_nextest_list_all() -> list[str]:
    """Inventory pass: also the only extraction (soldr#2933's rule, ported).

    `--archive-file` + `--extract-to` decompresses the whole archive into
    `$WORK/extract`; `nextest_list_selected` / `nextest_run` below reuse that
    extraction (or fall back to re-extracting) instead of paying for it
    twice, the same rule `nextest_reuse_extraction.py` encodes for the
    native path -- ported here as a `find`-based shallow search since the
    guest has no Python to run that script.
    """
    return [
        "stage_start nextest_list_all",
        'if [ "$FETCH_TESTS_OK" -eq 0 ] && [ "$EXTRACT_WORKSPACE_OK" -eq 0 ] && [ "$NEXTEST_VERSION_OK" -eq 0 ]; then',
        # nextest canonicalizes --extract-to before extracting and exits 96 when
        # it does not exist yet (seen live on release run 33820395040).
        '  mkdir -p "$WORK/extract"',
        '  "$NEXTEST_BIN" nextest list \\',
        '    --archive-file "$WORK/tests.tar.zst" \\',
        '    --extract-to "$WORK/extract" \\',
        '    --workspace-remap "$WORK/workspace" \\',
        "    --profile target-run \\",
        '    --message-format json-pretty > "$WORK/all-list.json" 2> "$WORK/all-list.stderr"',
        "  LA_RC=$?",
        '  if [ "$LA_RC" -eq 0 ]; then',
        "    record nextest_list_all pass",
        "    LIST_ALL_OK=0",
        "  else",
        '    record nextest_list_all fail "nextest list exited $LA_RC: $(tail -c 400 "$WORK/all-list.stderr")"',
        "    LIST_ALL_OK=1",
        "  fi",
        "else",
        '  record nextest_list_all fail "skipped: tests.tar.zst/workspace extraction/nextest_version not ready"',
        "  LIST_ALL_OK=1",
        "fi",
        "",
        "# Reuse flags for the two nextest passes below (soldr#2933, ported for",
        "# a guest with no Python to run nextest_reuse_extraction.py): a",
        "# shallow search for the reuse metadata nextest's own extraction",
        "# wrote, falling back to a second --archive-file/--extract-to pass",
        "# aimed at the same directory when it is not found.",
        'REUSE_BIN_META=""',
        'REUSE_CARGO_META=""',
        'if [ "$LIST_ALL_OK" -eq 0 ]; then',
        '  REUSE_BIN_META=$(find "$WORK/extract" -maxdepth 4 -type f -name binaries-metadata.json 2>/dev/null | head -n1)',
        '  REUSE_CARGO_META=$(find "$WORK/extract" -maxdepth 4 -type f -name cargo-metadata.json 2>/dev/null | head -n1)',
        "fi",
        'if [ -n "$REUSE_BIN_META" ] && [ -n "$REUSE_CARGO_META" ] && [ -d "$WORK/extract/target" ]; then',
        '  REUSE_ARGS="--binaries-metadata $REUSE_BIN_META --cargo-metadata $REUSE_CARGO_META --target-dir-remap $WORK/extract/target"',
        '  NEXTEST_TARGET_DIR="$WORK/extract/target"',
        "else",
        '  REUSE_ARGS="--archive-file $WORK/tests.tar.zst --extract-to $WORK/extract --extract-overwrite"',
        '  NEXTEST_TARGET_DIR="$WORK/extract/target"',
        "fi",
        "stage_end nextest_list_all",
        "",
    ]


def _stage_nextest_list_selected() -> list[str]:
    return [
        "stage_start nextest_list_selected",
        'if [ "$LIST_ALL_OK" -eq 0 ] && [ "$FETCH_FILTER_OK" -eq 0 ]; then',
        '  FILTER=$(cat "$WORK/filter.txt")',
        "  # shellcheck disable=SC2086",
        '  "$NEXTEST_BIN" nextest list $REUSE_ARGS \\',
        '    --workspace-remap "$WORK/workspace" \\',
        "    --profile target-run \\",
        "    --partition hash:1/1 \\",
        '    -E "$FILTER" \\',
        '    --message-format json-pretty > "$WORK/list.json" 2> "$WORK/list.stderr"',
        "  LS_RC=$?",
        '  if [ "$LS_RC" -eq 0 ]; then',
        "    record nextest_list_selected pass",
        "    LIST_SELECTED_OK=0",
        "  else",
        '    record nextest_list_selected fail "nextest list exited $LS_RC: $(tail -c 400 "$WORK/list.stderr")"',
        "    LIST_SELECTED_OK=1",
        "  fi",
        "else",
        '  record nextest_list_selected fail "skipped: nextest_list_all or filter.txt not ready"',
        "  LIST_SELECTED_OK=1",
        "fi",
        "stage_end nextest_list_selected",
        "",
    ]


def _stage_nextest_run() -> list[str]:
    return [
        "stage_start nextest_run",
        'if [ "$LIST_SELECTED_OK" -eq 0 ] && [ "$TOOLCHAIN_LINK_OK" -eq 0 ] \\',
        '  && [ "$FETCH_SOLDR_DAEMON_OK" -eq 0 ] && [ "$EXTRACT_FIXTURES_OK" -eq 0 ]; then',
        '  FILTER=$(cat "$WORK/filter.txt")',
        "  # --no-fail-fast, unlike the native lanes' --max-fail 3: one guest boot",
        "  # costs minutes, so a run must report every failure it can find.",
        "  # (Comments must stay OUT of the continued command below: PR #3087's",
        "  # third run split it and executed '--no-fail-fast' as a command.)",
        "  # shellcheck disable=SC2086",
        '  "$NEXTEST_BIN" nextest run $REUSE_ARGS \\',
        '    --workspace-remap "$WORK/workspace" \\',
        "    --profile target-run \\",
        "    --partition hash:1/1 \\",
        '    -E "$FILTER" \\',
        '    --no-fail-fast > "$WORK/nextest-run.log" 2>&1',
        "  NR_RC=$?",
        '  if [ "$NR_RC" -eq 0 ]; then',
        "    record nextest_run pass",
        "  else",
        '    record nextest_run fail "nextest run exited $NR_RC"',
        "  fi",
        "else",
        '  record nextest_run fail "skipped: nextest_list_selected/toolchain_link/soldr-daemon/fixtures not ready"',
        "fi",
        "stage_end nextest_run",
        "",
    ]


def _stage_collect_results() -> list[str]:
    """Copy only small diagnostics into /tmp/results/ -- never the extraction.

    `summary.txt` already lives at `/tmp/results/summary.txt` (every `record`
    call above wrote straight to it); everything else here is best-effort
    (`2>/dev/null`) so a missing file from an earlier failed stage does not
    itself fail the script -- the missing file is its own diagnostic once
    collected.
    """
    return [
        "stage_start collect_results",
        'cp "$WORK/toolchain-ensure.json" /tmp/results/ 2>/dev/null',
        'cp "$WORK/all-list.json" /tmp/results/ 2>/dev/null',
        'cp "$WORK/list.json" /tmp/results/ 2>/dev/null',
        "",
        "# The junit path depends on which nextest_run path actually executed",
        "# (target-dir-remap reuse vs. a fresh extract-to); check both rather",
        "# than assuming one.",
        'JUNIT_SRC=""',
        'if [ -f "$NEXTEST_TARGET_DIR/nextest/target-run/junit.xml" ]; then',
        '  JUNIT_SRC="$NEXTEST_TARGET_DIR/nextest/target-run/junit.xml"',
        'elif [ -f "$WORK/workspace/target/nextest/target-run/junit.xml" ]; then',
        '  JUNIT_SRC="$WORK/workspace/target/nextest/target-run/junit.xml"',
        "fi",
        'if [ -n "$JUNIT_SRC" ]; then',
        '  cp "$JUNIT_SRC" /tmp/results/junit.xml 2>/dev/null',
        "fi",
        "",
        'if [ -f "$WORK/nextest-run.log" ]; then',
        f'  tail -c {NEXTEST_LOG_TAIL_BYTES} "$WORK/nextest-run.log" > /tmp/results/nextest-run.log 2>/dev/null',
        "fi",
        "",
        "cp /tmp/diskutil-list.txt /tmp/diskutil-info.txt /tmp/erase.log /tmp/results/ 2>/dev/null",
        'cp "$WORK/all-list.stderr" /tmp/results/ 2>/dev/null',
        "df -h > /tmp/results/df.txt 2>&1",
        "env > /tmp/results/env.txt 2>&1",
        "stage_end collect_results",
        "",
    ]


def append_github_output_multiline(path: Path, name: str, value: str) -> None:
    """Append a multi-line `name<<EOF / value / EOF` block to `$GITHUB_OUTPUT`.

    Doing this in Python instead of a separate bash heredoc step keeps the
    workflow's inline `run:` footprint down and makes the delimiter handling
    testable instead of hand-typed YAML.
    """
    delimiter = f"GITHUB_OUTPUT_{name.upper()}_EOF"
    with path.open("a", encoding="utf-8") as handle:
        handle.write(f"{name}<<{delimiter}\n{value}{delimiter}\n")


def parse_summary(text: str) -> dict[str, tuple[bool, str]]:
    """Parse the guest's flat `key=value` results file.

    Shared shape with `smoke_release_artifacts.parse_summary`: each line is
    `name=pass[:detail]` or `name=fail[:detail]`. A malformed or truncated
    line (a wedged guest can be killed mid-write) is not silently dropped --
    it fails as its own diagnostic.
    """
    results: dict[str, tuple[bool, str]] = {}
    for lineno, raw in enumerate(text.splitlines(), start=1):
        line = raw.strip()
        if not line:
            continue
        name, sep, rest = line.partition("=")
        if not sep:
            results[f"summary_line_{lineno}"] = (False, f"malformed line: {raw!r}")
            continue
        status, _, detail = rest.partition(":")
        results[name] = (status == "pass", detail)
    return results


def verify_collected(collected_dir: Path, *, guest_exit_code: str) -> int:
    """Read the collected Recovery results and fail with a named diagnostic."""
    summary_path = collected_dir / RESULTS_FILE
    if not summary_path.is_file():
        sys.exit(
            f"ERROR: {summary_path} is missing — the Recovery guest never wrote "
            "results (did the script even reach a shell? check the action's "
            "workdir/results/*.ppm screendumps)."
        )
    results = parse_summary(summary_path.read_text(encoding="utf-8", errors="replace"))

    failures: list[str] = []
    for name in CHECKS:
        if name not in results:
            failures.append(f"{name}: no result recorded (guest script exited early?)")
            continue
        ok, detail = results[name]
        if not ok:
            failures.append(f"{name}: {detail}")

    if guest_exit_code.strip() != "0":
        failures.append(
            f"guest script exit code {guest_exit_code!r} != '0' "
            "(see the per-check results above for which check failed)"
        )

    if failures:
        joined = "\n  - ".join(failures)
        sys.exit(f"ERROR: macOS Recovery target-run smoke failed:\n  - {joined}")

    print(
        f"macOS Recovery target-run smoke OK: {len(results)} checks recorded, all passed"
    )
    return 0


def verify_replay_artifacts(
    collected_dir: Path,
    *,
    manifest: Path,
    repo_root: Path,
    target: str,
    github_summary: Path | None = None,
) -> int:
    """Run the same two checks the native `target-run` path runs, deferred.

    The Recovery guest cannot validate its own filter against a real nextest
    inventory before it boots (there is no Linux-side inventory yet, hence
    `target_run_ownership.py --filter-only`), so that validation -- a
    selector matching zero tests is stale -- runs here instead, against the
    `all-list.json` the guest's own `nextest_list_all` stage produced. The
    coverage reconciliation (`target_run_summary.py --require-junit`) is the
    same script + same flags the native path's step uses, run against the
    collected `list.json` / `junit.xml`.

    Invoked via subprocess rather than imported: the two scripts live under
    `.github/scripts/`, not `ci/`, and this mirrors exactly what the
    workflow step for the native path already runs, keeping this function's
    behavior identical to a passthrough of that shell.

    `scripts_dir` is resolved from this module's own location, not from
    `repo_root`: the two are the same soldr checkout in CI, but `repo_root`
    is the ownership manifest's *source-scan* root (what
    `validate_source_ownership` walks) and need not carry a `.github/`
    directory of its own in, say, a focused test fixture.
    """
    scripts_dir = Path(__file__).resolve().parents[1] / ".github" / "scripts"
    ownership_script = scripts_dir / "target_run_ownership.py"
    summary_script = scripts_dir / "target_run_summary.py"

    all_list = collected_dir / "all-list.json"
    list_json = collected_dir / "list.json"
    junit = collected_dir / "junit.xml"

    failures: list[str] = []

    if not all_list.is_file():
        failures.append(
            f"{all_list} is missing (nextest_list_all did not produce an inventory)"
        )
    else:
        filter_output = collected_dir / "_verify_filter.txt"
        result = subprocess.run(
            [
                sys.executable,
                str(ownership_script),
                "--manifest",
                str(manifest),
                "--repo-root",
                str(repo_root),
                "--list-json",
                str(all_list),
                "--target",
                target,
                "--filter-output",
                str(filter_output),
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        if result.returncode != 0:
            failures.append(
                "target-run ownership inventory validation failed:\n"
                f"{result.stdout}{result.stderr}".strip()
            )

    if not list_json.is_file():
        failures.append(
            f"{list_json} is missing (nextest_list_selected did not produce a list)"
        )
    else:
        summary_args = [
            sys.executable,
            str(summary_script),
            "--target",
            target,
            "--output",
            str(collected_dir / "target-run-summary.json"),
            "--list-json",
            str(list_json),
            "--junit",
            str(junit),
            "--require-junit",
        ]
        if github_summary is not None:
            summary_args += ["--github-summary", str(github_summary)]
        result = subprocess.run(
            summary_args, capture_output=True, text=True, check=False
        )
        if result.returncode != 0:
            failures.append(
                "target-run coverage summary failed:\n"
                f"{result.stdout}{result.stderr}".strip()
            )

    if failures:
        joined = "\n  - ".join(failures)
        sys.exit(
            "ERROR: macOS Recovery replay artifact verification failed:\n"
            f"  - {joined}"
        )

    print(
        "macOS Recovery replay artifacts verified: ownership inventory + "
        "coverage summary OK"
    )
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="subcommand", required=True)

    emit = subparsers.add_parser(
        "emit-guest-script", help="write the guest script that runs the replay"
    )
    emit.add_argument("--output", required=True, type=Path)
    emit.add_argument(
        "--github-output",
        default=None,
        type=Path,
        help=(
            "also append the guest script to this $GITHUB_OUTPUT file as a "
            "'script' multi-line output, so the workflow needs no separate "
            "heredoc step to hand it to the docker-mac-x64 action's `run:` "
            "input"
        ),
    )

    verify = subparsers.add_parser(
        "verify-collected", help="verify the guest's collected results"
    )
    verify.add_argument("--collected", required=True, type=Path)
    verify.add_argument("--guest-exit-code", required=True)
    verify.add_argument(
        "--manifest",
        type=Path,
        default=None,
        help=(
            "ci/target-run-ownership.json; when given (with --repo-root and "
            "--target), also validates the ownership inventory and "
            "coverage summary from the collected all-list.json/list.json/"
            "junit.xml"
        ),
    )
    verify.add_argument("--repo-root", type=Path, default=None)
    verify.add_argument("--target", default=None)
    verify.add_argument("--github-summary", type=Path, default=None)

    args = parser.parse_args(argv)

    if args.subcommand == "emit-guest-script":
        script_text = build_guest_script()
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(script_text, encoding="utf-8")
        print(f"wrote Recovery guest script to {args.output}")
        if args.github_output is not None:
            append_github_output_multiline(args.github_output, "script", script_text)
            print(f"appended 'script' output to {args.github_output}")
        return 0

    verify_collected(args.collected, guest_exit_code=args.guest_exit_code)
    if args.manifest is not None:
        if args.repo_root is None or args.target is None:
            parser.error("--manifest requires --repo-root and --target")
        return verify_replay_artifacts(
            args.collected,
            manifest=args.manifest,
            repo_root=args.repo_root,
            target=args.target,
            github_summary=args.github_summary,
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
