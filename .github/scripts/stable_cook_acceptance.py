#!/usr/bin/env python3
"""Run the stable-tree `soldr cook` three-mode acceptance matrix in Linux.

soldr#3043 Phase 2 needs the same shape of proof `dylint_cook_acceptance.py`
gives the Dylint dependency cook: run `soldr cook` against a real workspace
under every cache shape `actions/cache` can hand a CI job, and fail loudly
when the outcome does not match what that shape should produce. Copy the
OVERALL SHAPE of that script (module docstring, a BASH heredoc executed
inside the shared `soldr-perf-local` Docker runner via `ci/perf_local.py`,
JSON-per-scenario rows parsed back in `main()`, and a `GITHUB_STEP_SUMMARY`
table) rather than adding a `--tree` mode to it -- soldr#3042 is editing that
file in a parallel track.

The subject here is different from Dylint's in three ways:

* It cooks THIS repository, not the small `ci/fixtures/dylint-cache`
  fixture. `/repo` is mounted **read-only** by `ci/perf_local.py`
  (`type=bind,...,readonly` -- see `ci/perf_local.py`), and `soldr cook`
  rewrites crate roots in place while cargo-chef's skeleton is live
  (soldr#566), so the workspace is `tar`-copied into a writable scratch
  directory first, exactly as the Dylint script copies its fixture into
  `$WORK`.
* The three modes are defined by which `$SOLDR_CACHE_DIR` subpaths survive
  a cache round trip, because that is exactly what `actions/cache` controls
  in the real lane:
    - `cold`   -- nothing survives. Expected outcome: `built`.
    - `warm`   -- `cache/cook` + `state.sqlite3` survive (tar'd out and back,
      the way `actions/cache` actually round-trips a store), `target/` is
      wiped. Expected outcome: `hydrated` or `warm-skip`. This is the
      regression guard for T2's `resolve_target_dir` fix: without it, cook's
      restore path writes into `target/debug/` regardless of `--target`, so
      cargo -- which only reads `target/<triple>/debug/` once `--target` is
      explicit -- never sees what was restored.
    - `object_cache_only` -- `cache/zccache` (the per-unit compiler cache)
      survives, `cache/cook` + `state.sqlite3` do not, `target/` is wiped.
      Expected outcome: `built`. A warm per-unit object store must not be
      mistakable for a working cook (mirrors `dylint_cook_acceptance.py`'s
      `object_cache_only -> miss` case and soldr#3039 Tier-3).
* Outcome classification and the cook-archive byte count are NOT
  re-implemented here. The BASH below shells out to `python3` inside the
  container and imports `classify` / `cook_archive_bytes` /
  `COOK_SKIPPED_UNCOOKABLE_WORKSPACE` straight from `run_stable_cook.py` --
  the same module the real `_build-and-test.yml` step runs -- so a drift in
  either script's idea of "what counts as hydrated" cannot go unnoticed by
  the other (CLAUDE.md's Agent Code-Smell Reporting Rule: a test that
  re-implements the logic it is testing validates a copy, not the thing).

`run_with_watchdog` / `descendants` / `dump_stacks` are copied verbatim from
`dylint_cook_acceptance.py` so a hang produces stacks instead of a silent
CI timeout.
"""

from __future__ import annotations

import json
import os
import runpy
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

# The three scenario names, in run order, mapped to the outcomes
# (`run_stable_cook.classify`'s vocabulary) each cache shape must produce.
# Keyed as a plain dict rather than a set-valued list because scenario order
# IS part of the contract this acceptance checks (cold must run first to
# populate the store the other two scenarios then selectively keep).
EXPECTED_OUTCOMES: dict[str, frozenset[str]] = {
    "cold": frozenset({"built"}),
    "warm": frozenset({"hydrated", "warm-skip"}),
    "object_cache_only": frozenset({"built"}),
}

BASH = r"""
set -euo pipefail
export CARGO_HOME=/root/.cargo
export CARGO_PROFILE_DEV_DEBUG=2
export CARGO_PROFILE_DEV_STRIP=none
export SOLDR_CACHE_DIR=/tmp/stable-cook/cache
export SOLDR_CARGO_WAIT_TIMEOUT_SECS=0
export SOLDR_COMPILE_REPLY_TIMEOUT_SECS=3600
export SOLDR_DAEMON_TOKIO_CONSOLE=1
export SOLDR_DAEMON_TOKIO_CONSOLE_PUBLISH_INTERVAL_MS=20
SOLDR=/target/debug/soldr
REPO="$(pwd)"
WORK=/tmp/stable-cook/work
unset CARGO_TARGET_DIR
rm -rf /tmp/stable-cook
DIAGNOSTICS=/tmp/stable-cook/diagnostics
mkdir -p "$WORK" "$DIAGNOSTICS"

# `/repo` is read-only (ci/perf_local.py mounts it
# `type=bind,...,readonly`), and `soldr cook` rewrites crate roots in place
# while cargo-chef's skeleton is live (soldr#566), so the workspace has to
# live somewhere writable. `target/` and `.git/` are excluded: cook computes
# its own target tree, and neither the cook recipe hash
# (`compute_recipe_hash_proxy`, keyed on Cargo.lock) nor the cook-index key
# need `.git` to be present.
tar -C "$REPO" --exclude=target --exclude=.git -cf - . | tar -C "$WORK" -xf -
cd "$WORK"

TRIPLE="$("$SOLDR" rustc -vV | sed -n 's/^host: //p')"
test -n "$TRIPLE"
echo "host triple: $TRIPLE"

descendants() {
  local parent="$1" child
  while read -r child; do
    test -n "$child" || continue
    printf '%s\n' "$child"
    descendants "$child"
  done < <(pgrep -P "$parent" 2>/dev/null || true)
}

dump_stacks() {
  local root="$1" output="$2" pid exe
  {
    printf '%s\n' "$root"
    descendants "$root"
    pgrep -f '/target/debug/soldr|soldr-daemon|cargo-dylint|dylint-driver|rustc|zccache' \
      2>/dev/null || true
  } | sort -n -u | while read -r pid; do
    test -r "/proc/$pid/status" || continue
    exe="$(readlink -f "/proc/$pid/exe" || true)"
    echo "--- pid=$pid exe=$exe ---" >>"$output"
    file "$exe" >>"$output" 2>&1 || true
    readelf -S "$exe" 2>/dev/null |
      grep -E '\.(debug_info|debug_line|symtab)' >>"$output" || true
    timeout 15s gdb -q -n -batch \
      -ex "set pagination off" \
      -ex "set print thread-events off" \
      -ex "info threads" \
      -ex "thread apply all bt full 64" \
      -p "$pid" >>"$output" 2>&1 || true
  done
}

run_with_watchdog() {
  local name="$1"; shift
  local log="$DIAGNOSTICS/${name}.log"
  SOLDR_DAEMON_TOKIO_CONSOLE_RECORD_PATH="$DIAGNOSTICS/${name}.tokio" \
    "$@" >"$log" 2>&1 &
  local pid="$!" elapsed=0
  while kill -0 "$pid" 2>/dev/null; do
    sleep 10
    elapsed=$((elapsed + 10))
    if ((elapsed >= 1800)); then
      {
        echo "WATCHDOG: $name exceeded the 1800s absolute deadline"
        date -u
        echo "=== process tree ==="
        ps -eo pid,ppid,pgid,stat,etimes,wchan:32,args --forest
        echo "=== native stacks and symbol inventory ==="
      } >"$DIAGNOSTICS/${name}-stacks.txt"
      dump_stacks "$pid" "$DIAGNOSTICS/${name}-stacks.txt"
      kill -TERM "$pid" $(descendants "$pid") 2>/dev/null || true
      wait "$pid" || true
      cat "$log" >&2
      echo "watchdog timeout: $name" >&2
      return 124
    fi
  done
  set +e
  wait "$pid"
  local status="$?"
  set -e
  if [[ "$status" -ne 0 ]]; then
    cat "$log" >&2
  fi
  return "$status"
}

# Classification and the archive byte count are read straight out of
# `run_stable_cook.py` (imported, not re-implemented) -- see the module
# docstring above. `$WORK` is a full copy of the repo, so
# `.github/scripts/run_stable_cook.py` is present at the relative path used
# below.
classify_and_bytes() {
  local name="$1" status="$2"
  python3 - "$name" "$status" "$SOLDR_CACHE_DIR" "$DIAGNOSTICS/${name}.log" <<'PY'
import json
import sys
from pathlib import Path

sys.path.insert(0, ".github/scripts")
from run_stable_cook import (  # noqa: E402
    COOK_SKIPPED_UNCOOKABLE_WORKSPACE,
    classify,
    cook_archive_bytes,
)

name, status, cache_dir, log_path = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4]
archive_bytes = cook_archive_bytes(Path(cache_dir))
if status == COOK_SKIPPED_UNCOOKABLE_WORKSPACE:
    outcome, detail = "uncookable", "COOK_SKIPPED_UNCOOKABLE_WORKSPACE"
elif status != 0:
    outcome, detail = "failed", f"exit={status}"
else:
    text = Path(log_path).read_text(encoding="utf-8", errors="replace")
    outcome, detail = classify(text)
print(json.dumps({
    "name": name,
    "outcome": outcome,
    "detail": detail,
    "exit_code": status,
    "archive_bytes": archive_bytes,
}))
PY
}

run_case() {
  local name="$1"
  local started ended wall_ms status=0 row
  started="$(date +%s%3N)"
  run_with_watchdog "$name" "$SOLDR" cook --workspace --target "$TRIPLE" -- --all-targets \
    || status=$?
  ended="$(date +%s%3N)"
  wall_ms=$((ended - started))
  row="$(classify_and_bytes "$name" "$status")"
  jq -c --argjson wall_ms "$wall_ms" '. + {wall_ms: $wall_ms}' <<<"$row"
}

# --- cold: nothing survives -------------------------------------------------
"$SOLDR" daemon stop >/dev/null 2>&1 || true
rm -rf "$SOLDR_CACHE_DIR"
rm -rf target
"$SOLDR" daemon start
run_case cold

# --- warm: cache/cook + state.sqlite3 round-trip a save/restore, target/
# is wiped. This is the direct regression guard for T2's resolve_target_dir
# fix: hydrate must write into target/<triple>/debug, not target/debug.
"$SOLDR" daemon stop >/dev/null 2>&1 || true
# Named assertions, not a bare tar failure: cold's Phase 4 index
# (`index_cooked_artifact`) is best-effort (cook.rs) -- a warning, not an
# error -- so a silently-skipped index would otherwise kill this whole run
# at the tar below with no scenario row and nothing in the summary.
test -d "$SOLDR_CACHE_DIR/cache/cook"
test -f "$SOLDR_CACHE_DIR/state.sqlite3"
# `state.sqlite3*` and not `state.sqlite3`: the state store runs
# `PRAGMA journal_mode=WAL`, so a `-wal`/`-shm` pair can hold committed index
# rows whenever the shutdown above did not checkpoint. The real
# `actions/cache` step lists the same sidecars, and a snapshot that drops them
# would restore an index quietly missing its newest entries -- which presents
# as `warm` mysteriously reporting `built`. The glob is expanded inside
# $SOLDR_CACHE_DIR, so `tar -C` cannot be used for the create side.
(cd "$SOLDR_CACHE_DIR" && tar -cf /tmp/stable-cook/warm-snapshot.tar cache/cook state.sqlite3*)
rm -rf "$SOLDR_CACHE_DIR/cache/cook" "$SOLDR_CACHE_DIR"/state.sqlite3*
tar -C "$SOLDR_CACHE_DIR" -xf /tmp/stable-cook/warm-snapshot.tar
rm -rf target
"$SOLDR" daemon start
run_case warm
# target/ was wiped above and warm/warm-skip never compiles anything (that
# is the whole point of the short-circuit) -- the hydrate/warm-skip path is
# the ONLY possible writer here. A cold cook also produces host-only
# artifacts (proc-macros, build scripts) under target/debug/, so this
# invariant holds only because nothing ran a real compile in this scenario;
# do not "fix" this assertion using a cold-scenario intuition.
#
# The negative half is the load-bearing one and it guards BOTH halves of the
# soldr#3043 extraction-root fix, which had to land in two places because
# `soldr cook --target X` reaches the hydrate through two different argv:
#   1. `cook_hydrate::resolve_target_dir` joining the triple when `--target`
#      is in argv (Phase 2, `cargo chef cook --target X`);
#   2. `SOLDR_COOK_HYDRATE_TARGET`, which `run_cook` exports because Phase 1
#      is `cargo chef prepare` and cargo-chef's prepare takes no `--target`.
# Drop either and this scenario extracts the whole archive into target/debug
# as well -- a duplicate multi-GB tree Cargo never reads for a `--target`
# build, with the warm-cook marker landing where soldr#621's short-circuit
# cannot see it.
test -d "target/$TRIPLE/debug/deps"
test -n "$(ls -A "target/$TRIPLE/debug/deps")"
test ! -e target/debug/deps

# --- object_cache_only: only cache/zccache survives -------------------------
# The `-wal`/`-shm` sidecars go too, for the same reason the warm snapshot
# keeps them: leaving a WAL behind would leave index rows readable and this
# scenario's whole claim is that the cook index is gone.
"$SOLDR" daemon stop >/dev/null 2>&1 || true
rm -rf "$SOLDR_CACHE_DIR/cache/cook" "$SOLDR_CACHE_DIR"/state.sqlite3*
rm -rf target
"$SOLDR" daemon start
run_case object_cache_only

"$SOLDR" daemon stop >/dev/null 2>&1 || true
"""


def parse_rows(lines: list[str]) -> list[dict[str, object]]:
    """Parse the scenario rows the BASH prints, skipping non-JSON output.

    A row is kept only when it decodes to a JSON object carrying both `name`
    and `outcome` -- everything else on stdout (cargo-chef noise, the `host
    triple:` banner, watchdog diagnostics) is real Docker exec output, not a
    scenario result, and is dropped rather than mistaken for one.
    """
    rows: list[dict[str, object]] = []
    for line in lines:
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict) and "name" in value and "outcome" in value:
            rows.append(value)
    return rows


def outcome_accepted(mode: str, outcome: str) -> bool:
    """Whether `outcome` is one this mode's expected-outcome set allows."""
    return outcome in EXPECTED_OUTCOMES.get(mode, frozenset())


def render_summary(rows: list[dict[str, object]]) -> str:
    """Markdown table for `GITHUB_STEP_SUMMARY`, one row per scenario."""
    lines = [
        "## Stable-tree dependency cook (soldr#3043)\n",
        "\n",
        "| Scenario | Outcome | Detail | Wall ms | Exit code | Archive MiB |\n",
        "|---|---|---|---:|---:|---:|\n",
    ]
    for row in rows:
        archive_bytes = row.get("archive_bytes")
        mib = (
            f"{float(archive_bytes) / (1024 * 1024):.1f}"
            if isinstance(archive_bytes, (int, float))
            else ""
        )
        lines.append(
            f"| {row.get('name', '')} | {row.get('outcome', '')} | "
            f"{row.get('detail', '')} | {row.get('wall_ms', '')} | "
            f"{row.get('exit_code', '')} | {mib} |\n"
        )
    lines.append(
        "\nArchive size counts against soldr#3047's 2.0 GiB `cache/cook` "
        "allocation.\n"
    )
    return "".join(lines)


def main() -> int:
    common_result = subprocess.run(
        ["git", "rev-parse", "--path-format=absolute", "--git-common-dir"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    common = Path(common_result.stdout.strip()).resolve()
    source_root = common.parent if common.name == ".git" else ROOT
    perf_local = runpy.run_path(str(ROOT / "ci" / "perf_local.py"))
    runner = perf_local["runner_for"](source_root)
    runner_container = runner.container
    relative = ROOT.resolve().relative_to(source_root.resolve())
    workdir = "/repo" if relative == Path(".") else f"/repo/{relative.as_posix()}"
    build = subprocess.run(
        [
            sys.executable,
            str(ROOT / "ci" / "perf_local.py"),
            "car" + "go",
            "--config",
            'build.rustflags=["--cfg","tokio_unstable"]',
            "build",
            "-p",
            "soldr-cli",
            "--bin",
            "soldr",
            "--locked",
            "--features",
            "tokio-console",
        ],
        cwd=ROOT,
        check=False,
    )
    if build.returncode:
        return build.returncode
    command = [
        "docker",
        "exec",
        "-i",
        "-w",
        workdir,
        runner_container,
        "bash",
        "-s",
    ]
    output_lines: list[str] = []
    with subprocess.Popen(
        command,
        cwd=ROOT,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        bufsize=1,
    ) as process:
        assert process.stdin is not None
        assert process.stdout is not None
        process.stdin.write(BASH)
        process.stdin.close()
        for line in process.stdout:
            output_lines.append(line)
            print(line, end="", flush=True)
        returncode = process.wait()
    subprocess.run(
        [
            "docker",
            "cp",
            f"{runner_container}:/tmp/stable-cook/diagnostics/.",
            os.environ.get("RUNNER_TEMP", str(ROOT / "target"))
            + "/stable-cook-diagnostics",
        ],
        check=False,
    )
    if returncode:
        return returncode

    rows = parse_rows(output_lines)
    expected_names = list(EXPECTED_OUTCOMES)
    if [row.get("name") for row in rows] != expected_names:
        print(f"incomplete stable cook rows: {rows}", file=sys.stderr)
        return 2

    failed = False
    for row in rows:
        name = str(row.get("name"))
        outcome = str(row.get("outcome"))
        if outcome == "uncookable":
            print(
                f"::error title=stable cook acceptance::cook[{name}] hit "
                "COOK_SKIPPED_UNCOOKABLE_WORKSPACE (exit "
                f"{row.get('exit_code')}) -- a workspace path dependency the "
                "cargo-chef recipe cannot materialize. Exclude the offending "
                "workspace member with -p (soldr#3043 step 3) rather than "
                "relaxing this acceptance. See soldr#2791.",
                file=sys.stderr,
            )
            failed = True
            continue
        if outcome == "failed":
            print(
                f"::error title=stable cook acceptance::cook[{name}] exited "
                f"non-zero ({row.get('detail')}) for reasons other than "
                "COOK_SKIPPED_UNCOOKABLE_WORKSPACE.",
                file=sys.stderr,
            )
            failed = True
            continue
        if not outcome_accepted(name, outcome):
            print(
                f"unexpected outcome for {name!r}: {outcome!r} "
                f"(expected one of {sorted(EXPECTED_OUTCOMES[name])})",
                file=sys.stderr,
            )
            failed = True

    summary = render_summary(rows)
    print(summary)
    step_summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if step_summary:
        with open(step_summary, "a", encoding="utf-8") as output:
            output.write(summary)

    return 3 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
