#!/usr/bin/env python3
"""Run the real cargo-dylint 6.0.1 cache acceptance matrix in Docker/Linux."""

from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

BASH = r"""
set -euo pipefail
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE
export CARGO_HOME=/root/.cargo
export SOLDR_CACHE_DIR=/tmp/dylint-acceptance/cache
export SOLDR_DAEMON_SPAWN_RETRY_BUDGET_MS=120000
export SOLDR_FORCE_MANAGED_CARGO_SUBCOMMANDS=1
rm -rf /tmp/dylint-acceptance
mkdir -p /tmp/dylint-acceptance
trap 'rm -rf /tmp/dylint-acceptance' EXIT
SOLDR=/target/debug/soldr
REPO="$(pwd)"

cp -a "$REPO/ci/fixtures/dylint-cache" /tmp/dylint-acceptance/a
git init -q /tmp/dylint-acceptance/a
git config --global --add safe.directory /tmp/dylint-acceptance/a
git -C /tmp/dylint-acceptance/a rev-parse --git-dir >/dev/null
git -C /tmp/dylint-acceptance/a config user.email fixture@soldr.invalid
git -C /tmp/dylint-acceptance/a config user.name "Soldr Fixture"
git -C /tmp/dylint-acceptance/a add .
git -C /tmp/dylint-acceptance/a commit -qm fixture
(cd /tmp/dylint-acceptance/a && "$SOLDR" cargo dylint --version)
git -C /tmp/dylint-acceptance/a worktree add -q /tmp/dylint-acceptance/b HEAD

run_case() {
  name="$1"; work="$2"; target="$3"
  start="$(date +%s%3N)"
  if ! (cd "$work" && CARGO_TARGET_DIR="$target" "$SOLDR" cargo dylint --all); then
    echo "Dylint library target contents after failure:" >&2
    find "$target/dylint/libraries" -maxdepth 5 -type f -print 2>/dev/null | sort >&2 || true
    return 1
  fi
  end="$(date +%s%3N)"
  "$SOLDR" cache flush --json >/dev/null
  (cd "$work" && "$SOLDR" cache report --json) > "/tmp/dylint-acceptance/$name.json"
  jq -cn --arg name "$name" --argjson wall_ms "$((end-start))" \
    --slurpfile report "/tmp/dylint-acceptance/$name.json" \
    '{name:$name,wall_ms:$wall_ms,
      stats_present:($report[0].session_stats_present == true and
        ($report[0].last_session | type) == "object"),
      hits:($report[0].last_session.stats.hits // $report[0].last_session.hits // 0),
      misses:($report[0].last_session.stats.misses // $report[0].last_session.misses // 0)}'
}

run_case cold /tmp/dylint-acceptance/a /tmp/dylint-acceptance/target-a
run_case warm_same_target /tmp/dylint-acceptance/a /tmp/dylint-acceptance/target-a
rm -rf /tmp/dylint-acceptance/target-a
run_case warm_clean_target /tmp/dylint-acceptance/a /tmp/dylint-acceptance/target-a
run_case sibling_worktree /tmp/dylint-acceptance/b /tmp/dylint-acceptance/target-b
printf '\npub fn changed_source() -> usize { 7 }\n' >> /tmp/dylint-acceptance/b/src/lib.rs
run_case changed_source /tmp/dylint-acceptance/b /tmp/dylint-acceptance/target-b

rm -rf /tmp/dylint-acceptance/target-diagnostic
printf '\npub fn dylint_fixture_violation() {}\n' \
  >> /tmp/dylint-acceptance/a/src/lib.rs
for pass in cold replay; do
  output="/tmp/dylint-acceptance/diagnostic-$pass.log"
  (cd /tmp/dylint-acceptance/a && \
    CARGO_TARGET_DIR=/tmp/dylint-acceptance/target-diagnostic \
    "$SOLDR" cargo dylint --all 2>&1) | tee "$output"
  grep -F "soldr Dylint fixture diagnostic" "$output" >/dev/null
  rm -rf /tmp/dylint-acceptance/target-diagnostic
done
"""


def main() -> int:
    common_dir = subprocess.run(
        ["git", "rev-parse", "--path-format=absolute", "--git-common-dir"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    common = Path(common_dir.stdout.strip()).resolve()
    source_root = common.parent if common_dir.returncode == 0 and common.name == ".git" else ROOT
    relative = ROOT.resolve().relative_to(source_root.resolve())
    workdir = "/repo" if relative == Path(".") else f"/repo/{relative.as_posix()}"
    bootstrap = subprocess.run(
        [
            sys.executable,
            str(ROOT / "ci" / "perf_local.py"),
            "cargo",
            "build",
            "-p",
            "soldr-cli",
            "--bin",
            "soldr",
            "--locked",
        ],
        cwd=ROOT,
        check=False,
    )
    if bootstrap.returncode != 0:
        return bootstrap.returncode
    command = [
        "docker",
        "exec",
        "-i",
        "-w",
        workdir,
        "soldr-perf-local",
        "bash",
        "-s",
    ]
    try:
        result = subprocess.run(
            command, input=BASH, text=True, capture_output=True, check=False
        )
        print(result.stdout, end="")
        print(result.stderr, end="", file=sys.stderr)
        if result.returncode != 0:
            return result.returncode
        rows = []
        for line in result.stdout.splitlines():
            try:
                row = json.loads(line)
            except json.JSONDecodeError:
                continue
            if (
                isinstance(row, dict)
                and {
                    "name",
                    "wall_ms",
                    "stats_present",
                    "hits",
                    "misses",
                }
                <= row.keys()
            ):
                rows.append(row)
        expected = [
            "cold",
            "warm_same_target",
            "warm_clean_target",
            "sibling_worktree",
            "changed_source",
        ]
        if [row["name"] for row in rows] != expected:
            print(f"error: incomplete scenario output: {rows}", file=sys.stderr)
            return 2
        by_name = {row["name"]: row for row in rows}
        checks = [
            (
                all(
                    row["stats_present"]
                    and isinstance(row["hits"], int)
                    and isinstance(row["misses"], int)
                    for row in rows
                ),
                "every scenario must have integer session stats",
            ),
            (by_name["cold"]["misses"] > 0, "cold run must report misses"),
            (by_name["warm_clean_target"]["hits"] > 0, "clean-target rebuild must hit"),
            (by_name["sibling_worktree"]["hits"] > 0, "sibling worktree must hit"),
            (by_name["changed_source"]["hits"] > 0, "changed source must retain hits"),
            (
                by_name["changed_source"]["misses"] > 0,
                "changed source must miss changed units",
            ),
        ]
        for passed, message in checks:
            if not passed:
                print(f"error: {message}: {rows}", file=sys.stderr)
                return 3
        summary = os.environ.get("GITHUB_STEP_SUMMARY")
        if summary:
            with open(summary, "a", encoding="utf-8") as output:
                output.write("## Dylint 6.0.1 cache acceptance\n\n")
                output.write(
                    "| Scenario | Wall ms | Hits | Misses |\n|---|---:|---:|---:|\n"
                )
                for row in rows:
                    output.write(
                        f"| {row['name']} | {row['wall_ms']} | {row['hits']} | {row['misses']} |\n"
                    )
        return 0
    except OSError as error:
        print(f"error: failed to execute Docker acceptance: {error}", file=sys.stderr)
        return 4


if __name__ == "__main__":
    sys.exit(main())
