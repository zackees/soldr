#!/usr/bin/env python3
"""Assert the full nextest archive test build is warm-cacheable.

This is intentionally a Docker harness. The source tree is bind-mounted,
but Cargo's target dir, CARGO_HOME, and soldr home live on Linux Docker
volumes so Cargo mtimes and zccache state are not distorted by Windows
bind-mount behavior.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import time
from collections import deque
from pathlib import Path
from typing import Any

REPO_ROOT = Path(__file__).resolve().parents[1]
IMAGE = "soldr-cook-dev"
DOCKERFILE = REPO_ROOT / "docker" / "cook-shared-cache" / "Dockerfile"

BASH_SCRIPT = r"""
set -euo pipefail

export CARGO_HOME=/root/.cargo
export CARGO_TERM_COLOR=always
export RUST_BACKTRACE=1
export SOLDR_DAEMON_SPAWN_RETRY_BUDGET_MS="${SOLDR_DAEMON_SPAWN_RETRY_BUDGET_MS:-120000}"

echo "## environment"
rustc --version
cargo --version

echo "## bootstrap soldr"
export CARGO_TARGET_DIR=/root/.soldr/bootstrap-target
cargo build -p soldr-cli --bin soldr --locked
SOLDR_BIN=/root/.soldr/bootstrap-target/debug/soldr
"$SOLDR_BIN" --version

DIAGNOSTICS_DIR=/tmp/soldr-cacheability
CACHE="$DIAGNOSTICS_DIR/root"
ARCHIVE_DIR="$DIAGNOSTICS_DIR/archives"
export SOLDR_CACHE_DIR="$CACHE"
rm -rf "$CACHE" "$ARCHIVE_DIR" /tmp/cold-report.json /tmp/warm-report.json
mkdir -p "$CACHE" "$ARCHIVE_DIR"

export CARGO_TARGET_DIR=/work/target

print_daemon_diagnostics() {
  echo "## soldr daemon diagnostics" >&2
  cat "$DIAGNOSTICS_DIR/soldr-daemon-status.json" >&2 || true
  cat "$DIAGNOSTICS_DIR/soldr-daemon-status.err" >&2 || true
  if [ -f "$CACHE/daemon-spawn.log" ]; then
    echo "## daemon-spawn.log tail" >&2
    tail -n 200 "$CACHE/daemon-spawn.log" >&2 || true
  fi
  echo "## soldr processes" >&2
  ps -ef | grep -E '[s]oldr|[z]ccache' >&2 || true
  echo "## retained diagnostic files" >&2
  find "$DIAGNOSTICS_DIR" -maxdepth 4 -type f | wc -l | \
    xargs printf 'file count: %s\n' >&2 || true
  du -sh "$DIAGNOSTICS_DIR" >&2 || true
  find "$DIAGNOSTICS_DIR" -maxdepth 4 -type f -printf '%p %s bytes\n' | \
    sort | head -n 200 >&2 || true
}

on_exit() {
  status=$?
  trap - EXIT
  if [ "$status" -ne 0 ]; then
    set +e
    "$SOLDR_BIN" daemon status --json \
      > "$DIAGNOSTICS_DIR/soldr-daemon-status.json" \
      2> "$DIAGNOSTICS_DIR/soldr-daemon-status.err"
    print_daemon_diagnostics
  fi
  exit "$status"
}
trap on_exit EXIT

# Resolve the cargo-nextest front-door tool before starting the daemon.  The
# first-use fetch/bootstrap path can restart the managed process while Cargo
# is already compiling; that obscures the cacheability check with a daemon
# lifecycle failure.  Subsequent archive builds exercise only compilation and
# cache traffic. Install the failure trap first so bootstrap failures retain
# the same diagnostics as archive failures.
echo "## prefetch cargo-nextest"
"$SOLDR_BIN" cargo nextest --version

ensure_soldr_daemon() {
  echo "## ensure soldr daemon"
  "$SOLDR_BIN" daemon start || true
  for _ in $(seq 1 120); do
    "$SOLDR_BIN" daemon status --json \
      > "$DIAGNOSTICS_DIR/soldr-daemon-status.json" \
      2> "$DIAGNOSTICS_DIR/soldr-daemon-status.err" || true
    if jq -e '.running == true' "$DIAGNOSTICS_DIR/soldr-daemon-status.json" > /dev/null 2>&1; then
      cat "$DIAGNOSTICS_DIR/soldr-daemon-status.json"
      return 0
    fi
    sleep 1
  done
  echo "soldr daemon did not report running" >&2
  print_daemon_diagnostics
  return 1
}

stop_soldr_daemon() {
  echo "## stop soldr daemon"
  "$SOLDR_BIN" daemon stop || true
  for _ in $(seq 1 60); do
    "$SOLDR_BIN" daemon status --json \
      > "$DIAGNOSTICS_DIR/soldr-daemon-status.json" \
      2> "$DIAGNOSTICS_DIR/soldr-daemon-status.err" || true
    if jq -e '.running == false' "$DIAGNOSTICS_DIR/soldr-daemon-status.json" > /dev/null 2>&1; then
      cat "$DIAGNOSTICS_DIR/soldr-daemon-status.json"
      return 0
    fi
    sleep 1
  done
  echo "soldr daemon did not stop" >&2
  print_daemon_diagnostics
  return 1
}

clean_target() {
  # Cargo tries to remove the target-dir root. In this Docker harness that
  # root is a volume mount point, so Docker Desktop can report EBUSY after
  # Cargo has removed the contents. Keep the cleanup deterministic by
  # emptying the mount point without deleting the mount point itself.
  cargo clean || true
  find "$CARGO_TARGET_DIR" -mindepth 1 -maxdepth 1 -exec rm -rf {} +
}

clean_target
ensure_soldr_daemon

echo "## cold nextest archive build"
cold_start=$(date +%s%3N)
CARGO_PROFILE_TEST_DEBUG=line-tables-only \
  "$SOLDR_BIN" cargo nextest archive --workspace --locked \
  --cargo-profile ci-nextest \
  --archive-file "$ARCHIVE_DIR/cold-tests.tar.zst" \
  --archive-format tar-zst
cold_end=$(date +%s%3N)
"$SOLDR_BIN" cache flush --json
"$SOLDR_BIN" cache report --json > /tmp/cold-report.json
ls -lh "$ARCHIVE_DIR/cold-tests.tar.zst"
"$SOLDR_BIN" cache shutdown --no-wait --json || true
stop_soldr_daemon

echo "## warm nextest archive build after cargo clean and daemon restart"
clean_target
ensure_soldr_daemon
warm_start=$(date +%s%3N)
CARGO_PROFILE_TEST_DEBUG=line-tables-only \
  "$SOLDR_BIN" cargo nextest archive --workspace --locked \
  --cargo-profile ci-nextest \
  --archive-file "$ARCHIVE_DIR/warm-tests.tar.zst" \
  --archive-format tar-zst
warm_end=$(date +%s%3N)
"$SOLDR_BIN" cache flush --json
"$SOLDR_BIN" cache report --json > /tmp/warm-report.json
ls -lh "$ARCHIVE_DIR/warm-tests.tar.zst"
"$SOLDR_BIN" cache shutdown --no-wait --json || true
stop_soldr_daemon

stat_json() {
  local report="$1"
  local key="$2"
  jq -r --arg k "$key" '.last_session.stats[$k] // .last_session[$k] // 0' "$report"
}

cold_hits="$(stat_json /tmp/cold-report.json hits)"
cold_misses="$(stat_json /tmp/cold-report.json misses)"
cold_non_cacheable="$(stat_json /tmp/cold-report.json non_cacheable)"
cold_hit_rate="$(stat_json /tmp/cold-report.json hit_rate)"
warm_hits="$(stat_json /tmp/warm-report.json hits)"
warm_misses="$(stat_json /tmp/warm-report.json misses)"
warm_non_cacheable="$(stat_json /tmp/warm-report.json non_cacheable)"
warm_hit_rate="$(stat_json /tmp/warm-report.json hit_rate)"

result="$(
  jq -cn \
    --argjson cold_hits "$cold_hits" \
    --argjson cold_misses "$cold_misses" \
    --argjson cold_non_cacheable "$cold_non_cacheable" \
    --argjson cold_hit_rate "$cold_hit_rate" \
    --argjson warm_hits "$warm_hits" \
    --argjson warm_misses "$warm_misses" \
    --argjson warm_non_cacheable "$warm_non_cacheable" \
    --argjson warm_hit_rate "$warm_hit_rate" \
    '{
      cold_hits: $cold_hits,
      cold_misses: $cold_misses,
      cold_non_cacheable: $cold_non_cacheable,
      cold_hit_rate: $cold_hit_rate,
      warm_hits: $warm_hits,
      warm_misses: $warm_misses,
      warm_non_cacheable: $warm_non_cacheable,
      warm_hit_rate: $warm_hit_rate
    }'
)"
echo "CACHEABILITY_RESULT $result"

if (( warm_hits <= 0 )); then
  echo "CACHEABILITY_FAILURE warm run reported zero hits" >&2
  exit 2
fi
if (( warm_misses != 0 )); then
  echo "CACHEABILITY_FAILURE warm run had misses; expected zero" >&2
  exit 3
fi

echo "CACHEABILITY_OK warm run had hits and zero misses"
echo "TIMING_MS cold=$((cold_end - cold_start)) warm=$((warm_end - warm_start))"
"""


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Run a Docker/Linux cold-clean-warm nextest archive build and "
            "fail if the warm zccache report has any misses."
        )
    )
    parser.add_argument(
        "--image",
        default=IMAGE,
        help=f"Docker image tag to build/use (default: {IMAGE})",
    )
    parser.add_argument(
        "--keep-volumes",
        action="store_true",
        help="keep the temporary Docker volumes for post-failure inspection",
    )
    parser.add_argument(
        "--suffix",
        default=None,
        help="override the Docker volume suffix; default is timestamp + pid",
    )
    return parser.parse_args(argv)


class PhaseTracker:
    """Turn the harness's ``## <name>`` markers into observable phases.

    soldr#1978 item 4. The acceptance is ~39-47 minutes and >99% of it lands
    in a single Actions step, so a failure says only "the 40-minute step
    failed" -- you re-run 40 minutes to find out where. The harness already
    announces every stage on stdout as ``## cold nextest archive build`` and
    friends; nothing was reading them.

    This folds each stage into a collapsible Actions group and records how
    long it took, so the log has navigable sections and the job summary ends
    with a timing table. Crucially it also remembers the phase that was open
    when output stopped: on failure that name is the single most useful fact,
    and it is exactly what the opaque step could never report.

    Grouping markers are emitted only under Actions -- locally they would be
    noise, since a terminal already shows the ``##`` lines in context.
    """

    MARKER = "## "

    def __init__(self, clock=time.monotonic, emit_groups: bool = False) -> None:
        self._clock = clock
        self._emit_groups = emit_groups
        self._started_at: float | None = None
        self.current: str | None = None
        self.phases: list[tuple[str, float]] = []

    def feed(self, line: str) -> str | None:
        """Consume one harness line; return a control line to print, if any."""
        if not line.startswith(self.MARKER):
            return None
        name = line[len(self.MARKER) :].strip()
        if not name:
            return None
        closing = self._close()
        self.current = name
        self._started_at = self._clock()
        if not self._emit_groups:
            return None
        # The close has to precede the open or Actions nests the groups.
        return f"{closing or ''}::group::{name}"

    def finish(self) -> None:
        """Close the open phase, if any. Safe to call more than once."""
        self._close()
        self.current = None

    def _close(self) -> str | None:
        if self.current is None or self._started_at is None:
            return None
        self.phases.append((self.current, self._clock() - self._started_at))
        self.current = None
        self._started_at = None
        return "::endgroup::\n" if self._emit_groups else None

    def record(self, name: str, seconds: float) -> None:
        """Record a phase measured outside the harness stream."""
        self.phases.append((name, seconds))

    def summary_markdown(self, failed_phase: str | None = None) -> str:
        """A phase-timing table for the job summary."""
        lines = ["### Cacheability phases", "", "| phase | duration |", "|---|---:|"]
        for name, seconds in self.phases:
            lines.append(f"| {name} | {format_duration(seconds)} |")
        total = sum(seconds for _, seconds in self.phases)
        lines.append(f"| **total** | **{format_duration(total)}** |")
        if failed_phase:
            lines += ["", f"**Failed during:** `{failed_phase}`"]
        return "\n".join(lines) + "\n"


def format_duration(seconds: float) -> str:
    minutes, secs = divmod(int(seconds), 60)
    return f"{minutes}m {secs:02d}s" if minutes else f"{secs}s"


def write_step_summary(markdown: str) -> None:
    """Append to the Actions job summary; a no-op off Actions."""
    path = os.environ.get("GITHUB_STEP_SUMMARY")
    if not path:
        return
    try:
        with open(path, "a", encoding="utf-8") as handle:
            handle.write(markdown)
    except OSError as err:  # a summary must never fail the acceptance
        print(f"warning: could not write job summary: {err}", file=sys.stderr)


def on_github_actions() -> bool:
    return os.environ.get("GITHUB_ACTIONS") == "true"


def docker_available() -> bool:
    try:
        result = subprocess.run(
            ["docker", "info"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
            timeout=20,
        )
    except (OSError, subprocess.TimeoutExpired):
        return False
    return result.returncode == 0


def build_image(image: str) -> int:
    return subprocess.run(
        ["docker", "build", "-f", str(DOCKERFILE), "-t", image, str(REPO_ROOT)],
        check=False,
    ).returncode


def run_harness(
    image: str, volumes: list[str], tracker: "PhaseTracker | None" = None
) -> tuple[int, dict[str, Any] | None]:
    cmd = [
        "docker",
        "run",
        "--rm",
        "--init",
        "-i",
        "-v",
        f"{REPO_ROOT}:/work",
        "-v",
        f"{volumes[0]}:/work/target",
        "-v",
        f"{volumes[1]}:/root/.cargo",
        "-v",
        f"{volumes[2]}:/root/.soldr",
        "-v",
        f"{volumes[3]}:/tmp/soldr-cacheability",
        "-w",
        "/work",
        image,
        "bash",
        "-s",
    ]
    print("+ " + " ".join(cmd), flush=True)
    process = subprocess.Popen(
        cmd,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )
    assert process.stdin is not None
    assert process.stdout is not None

    try:
        payload = BASH_SCRIPT.replace("\r\n", "\n").replace("\r", "\n")
        process.stdin.write(payload.encode("utf-8"))
        process.stdin.close()
    except BrokenPipeError:
        pass

    result: dict[str, object] | None = None
    tail: deque[str] = deque(maxlen=80)
    for raw_line in process.stdout:
        line = raw_line.decode("utf-8", errors="replace")
        if tracker is not None:
            control = tracker.feed(line)
            if control:
                print(control, flush=True)
        print(line, end="", flush=True)
        tail.append(line)
        if line.startswith("CACHEABILITY_RESULT "):
            payload = line.removeprefix("CACHEABILITY_RESULT ").strip()
            result = json.loads(payload)

    code = process.wait()
    if tracker is not None and code == 0:
        # Leave the phase open on failure so the summary can name it.
        tracker.finish()
    if code != 0:
        print("\nlast harness output:", file=sys.stderr)
        for line in tail:
            print(line, end="", file=sys.stderr)
    return code, result


def remove_volumes(volumes: list[str]) -> None:
    subprocess.run(["docker", "volume", "rm", "--force", *volumes], check=False)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    if not docker_available():
        print("error: docker is not available or the daemon is not reachable", file=sys.stderr)
        return 2

    suffix = args.suffix or f"{int(time.time())}-{os.getpid()}"
    volumes = [
        f"soldr-nextest-cacheability-target-{suffix}",
        f"soldr-nextest-cacheability-cargo-{suffix}",
        f"soldr-nextest-cacheability-home-{suffix}",
        f"soldr-nextest-cacheability-diagnostics-{suffix}",
    ]
    print("Docker volumes:")
    for volume in volumes:
        print(f"  {volume}")

    tracker = PhaseTracker(emit_groups=on_github_actions())
    try:
        # soldr#1978 item 4: the image build is a phase in its own right --
        # `--pull` with no layer cache means it can dominate a run, and until
        # now it was indistinguishable from the acceptance it precedes.
        build_started = time.monotonic()
        image_code = build_image(args.image)
        tracker.record("docker build", time.monotonic() - build_started)
        if image_code != 0:
            return image_code
        code, result = run_harness(args.image, volumes, tracker)
        if code != 0:
            return code
        if result is None:
            print("error: harness did not emit CACHEABILITY_RESULT", file=sys.stderr)
            return 4
        if int(result.get("warm_hits", 0)) <= 0:
            print("error: warm run reported zero hits", file=sys.stderr)
            return 5
        if int(result.get("warm_misses", 0)) != 0:
            print(f"error: warm run had misses: {result}", file=sys.stderr)
            return 6
        return 0
    finally:
        # The summary matters most when something failed, so it is written
        # here rather than on the success path: `failed_phase` names the
        # stage that was still open, which is the fact a single opaque step
        # could never give you.
        failed_phase = tracker.current
        tracker.finish()
        write_step_summary(tracker.summary_markdown(failed_phase))
        if failed_phase:
            print(f"failed during phase: {failed_phase}", file=sys.stderr)
        if args.keep_volumes:
            print("Keeping Docker volumes for inspection.")
        else:
            remove_volumes(volumes)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
