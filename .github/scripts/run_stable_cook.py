#!/usr/bin/env python3
"""Cook the stable dependency tree for one target, as a single CI entry point.

soldr#3043 Phase 2: `_build-and-test.yml` must stay orchestration-only
(CLAUDE.md bans complex inline CI logic), so the `soldr cook` invocation for
the stable host-validation tree -- argv construction, exit-code triage, and
outcome reporting -- lives here instead of in the workflow YAML.

Exit codes are keyed to the constants `soldr cook` itself defines
(crates/soldr-cli/src/cook.rs):

    0   success (cook ran, or classified `built`/`hydrated`/`warm-skip`/
        `restore-declined`; see `classify()`)
    3   COOK_SKIPPED_UNCOOKABLE_WORKSPACE -- a path dependency the cargo-chef
        recipe cannot materialise. This is a hard failure here: per
        soldr#3043 step 3 the fix is to exclude the offending workspace
        member with `-p`, not to relax the guard.
    4   `--require-warm` was passed and the run neither hydrated from a
        prior cook artifact nor warm-skipped Phase 2. Without `--require-warm`
        this case only emits a `::warning` annotation -- the acceptance
        number is not measurable until soldr#3040's analyzer lands.
    5   COOK_ARTIFACT_NOT_INDEXED -- cook built and packed the archive but
        its closing `CookRecord` found no daemon, so no index row names the
        artifact and no later run can hydrate from it (soldr#3117). `soldr
        cook` itself only warns here, because a missing daemon must not fail
        a developer's local cook; in this lane the index row IS the product,
        so an unindexed artifact is a dead mechanism and fails the step.
    N   any other non-zero exit from `soldr cook` itself, propagated as-is.

This is a diagnostic layer over `soldr cook`, not a reimplementation of it:
every classification is read back out of cook's own stderr markers, which are
quoted verbatim below rather than re-derived.
"""

from __future__ import annotations

import argparse
import os
import signal
import subprocess
import sys
import threading
import time
from collections.abc import Callable, Sequence
from pathlib import Path

# Sibling import (soldr#2120 pattern, see dylint_toolchain_channel.py):
# `cook_inspect.py` lives next to this file and both must resolve regardless
# of whether this module is run as `__main__`, imported by
# `stable_cook_acceptance.py`'s `sys.path.insert(0, ".github/scripts")`, or
# loaded by `_script_loader.load_script_module` in tests.
_SCRIPT_DIR = Path(__file__).resolve().parent
if str(_SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(_SCRIPT_DIR))

import cook_inspect  # noqa: E402  pylint: disable=wrong-import-position

# soldr cook's own literal markers. Do not invent new spellings here --
# these are exactly the strings the binary emits, and drift silently breaks
# classification without touching cook's own tests.
#
#   HYDRATE_MARKER: emit_hydrate_line(),
#     crates/soldr-cli/src/cargo_front_door/cook_hydrate.rs
#   WARM_SKIP_MARKER: the soldr#621 warm-cook marker check,
#     crates/soldr-cli/src/cook.rs
#   DECISION_SKIP_MARKER: decide_cook_restore()'s Skip branch,
#     crates/soldr-cli/src/cargo_front_door/cook_hydrate.rs
#   UNINDEXED_MARKER: the CookRecord failure branch of
#     index_cooked_artifact_with_packer(), crates/soldr-cli/src/cook.rs
HYDRATE_MARKER = "soldr cook: auto-hydrate activated"
WARM_SKIP_MARKER = "soldr cook: warm-cook detected"
DECISION_SKIP_MARKER = "soldr cook: decision=skip"
UNINDEXED_MARKER = "CookRecord to daemon failed"

# crates/soldr-cli/src/cook.rs: `const COOK_SKIPPED_UNCOOKABLE_WORKSPACE: i32 = 3;`
COOK_SKIPPED_UNCOOKABLE_WORKSPACE = 3
# This script's own exit code for a `--require-warm` violation. Not a soldr
# constant -- soldr cook itself always exits 0 for a `built`/`restore-declined`
# outcome; the failure is this wrapper's opinion, gated behind the flag.
REQUIRE_WARM_FAILURE = 4
# This script's own exit code for a built-but-unindexed archive (soldr#3117).
# Also not a soldr constant: cook exits 0 and warns, for the reason given in
# the module docstring.
COOK_ARTIFACT_NOT_INDEXED = 5

# `--all-targets` is what makes cargo-chef build dev-dependencies, which the
# ci-test stable tree needs (clippy `--all-targets`, nextest `--lib --tests`).
# It is the default here rather than something the workflow must remember to
# pass.
DEFAULT_CHEF_ARGS: tuple[str, ...] = ("--all-targets",)

# Wall-clock ceiling for the whole `soldr cook` invocation. `soldr cook`'s own
# in-process no-progress watchdog (`SOLDR_COOK_NO_PROGRESS_SECS`, default
# 900s) fails fast on a genuine stall; this is the outer backstop for the
# rarer case where soldr itself is wedged and never reaches its own
# watchdog loop (e.g. blocked before the first cargo-chef phase starts).
DEFAULT_TIMEOUT_SECS = 5400
# Grace period between SIGQUIT and SIGTERM, and between SIGTERM and giving up
# and reporting (the process may still be alive after this; we do not SIGKILL
# by default so a genuine core dump from SIGQUIT has time to flush).
TIMEOUT_GRACE_SECS = 30
# Exit code this wrapper uses for its own outer timeout, distinct from any
# `soldr cook` exit code and from `REQUIRE_WARM_FAILURE` / `COOK_ARTIFACT_NOT_INDEXED`.
TIMEOUT_EXIT_CODE = 124

# How often (seconds) to run a non-fatal `cook_inspect` capture of the whole
# cook process tree while `soldr cook` is still running, so a healthy-but-
# slow run (measured 23-37 minutes on main) leaves a trail of periodic
# on-CPU/off-CPU snapshots instead of CI staying silent until the outer
# `--timeout-secs` ceiling trips. `0` disables periodic inspection entirely;
# the final capture at the hard `--timeout-secs` ceiling (below) is
# unconditional and does not depend on this value.
DEFAULT_INSPECT_EVERY_SECS = 600.0

Runner = Callable[[list[str], Path], subprocess.CompletedProcess[str]]
# `(root_pid, capture_dir) -> report_dir`. Defaults to `cook_inspect.run_inspection`;
# injectable so tests never need real `perf`/`gdb`.
Inspector = Callable[[int, Path], Path]


def default_inspect_dir() -> Path:
    """`$RUNNER_TEMP/cook-inspect` in CI, `./cook-inspect` for a local run."""
    runner_temp = os.environ.get("RUNNER_TEMP")
    if runner_temp:
        return Path(runner_temp) / "cook-inspect"
    return Path("cook-inspect")


def default_inspector(root_pid: int, capture_dir: Path) -> Path:
    return cook_inspect.run_inspection(root_pid, capture_dir)


def capture_dir_name(seq: int, elapsed_secs: float) -> str:
    """`<inspect-dir>/capture-NNN-<elapsed>s/` per capture, so periodic and
    final captures never collide and sort in chronological order."""
    return f"capture-{seq:03d}-{int(elapsed_secs)}s"


def next_tick_number_due(
    elapsed_secs: float, interval_secs: float, ticks_fired: int
) -> int | None:
    """The next periodic-inspection tick number (1-indexed) due at
    `elapsed_secs`, or `None` if no new tick is due yet.

    Pure and clock-injectable on purpose (no real sleeping needed to test the
    scheduling math): tick `k` is due once `elapsed_secs >= k * interval_secs`.
    If more than one interval has elapsed since the last check (the caller was
    busy, or a previous capture ran long), this jumps straight to the highest
    due tick number rather than replaying every missed one -- an overrun
    skips the ticks it missed instead of queuing a burst of catch-up
    captures.
    """
    if interval_secs <= 0:
        return None
    total_due = int(elapsed_secs // interval_secs)
    if total_due > ticks_fired:
        return total_due
    return None


def default_runner(command: list[str], cwd: Path) -> subprocess.CompletedProcess[str]:
    """Run `command` from `cwd`, capturing both streams as text.

    Kept for callers (and historical tests) that want simple capture with no
    live streaming or timeout; `main()`'s own default path is
    `stream_and_capture`, below, which is what CI actually exercises.
    """
    return subprocess.run(command, cwd=cwd, capture_output=True, text=True, check=False)


def _pump_stream(pipe, sink, chunks: list[str]) -> None:
    """Copy `pipe` line-by-line into `sink` (live) and `chunks` (captured)."""
    try:
        for line in iter(pipe.readline, ""):
            sink.write(line)
            sink.flush()
            chunks.append(line)
    finally:
        pipe.close()


def _terminate_with_grace(proc: subprocess.Popen, grace_secs: float) -> None:
    """SIGQUIT (so a stuck child can dump state), then SIGTERM, each with a
    bounded grace period. Never SIGKILL here -- a hung `soldr` process may be
    holding a lock or writing a dump the SIGQUIT itself triggered, and a
    stray SIGKILL would truncate it."""
    quit_signal = getattr(signal, "SIGQUIT", signal.SIGTERM)
    try:
        proc.send_signal(quit_signal)
    except OSError:
        pass
    try:
        proc.wait(timeout=grace_secs)
        return
    except subprocess.TimeoutExpired:
        pass
    try:
        proc.terminate()
    except OSError:
        pass
    try:
        proc.wait(timeout=grace_secs)
    except subprocess.TimeoutExpired:
        pass


def _report_capture(
    stderr_sink,
    capture_dir: Path,
    label: str,
    *,
    warn: bool,
    elapsed_secs: float,
) -> None:
    """Print one capture's `REPORT.txt` (falling back to `SUMMARY.md`) into a
    `::group::` on `stderr_sink`, and -- for a periodic (non-final) capture
    only -- follow it with a non-fatal `::warning`. The final capture (right
    before the hard `--timeout-secs` kill) is reported the same way but
    without its own warning line; the hard-timeout `::error` that follows it
    already names the capture directory.
    """
    report_path = capture_dir / "REPORT.txt"
    if not report_path.is_file():
        report_path = capture_dir / "SUMMARY.md"
    text = ""
    try:
        text = report_path.read_text(encoding="utf-8", errors="replace")
    except OSError as error:
        text = f"(inspector report unreadable: {error})\n"

    stderr_sink.write(f"::group::soldr cook inspector -- {label} ({capture_dir})\n")
    stderr_sink.write(text)
    if not text.endswith("\n"):
        stderr_sink.write("\n")
    stderr_sink.write("::endgroup::\n")
    if warn:
        stderr_sink.write(
            f"::warning title=soldr cook::cook still running after {elapsed_secs:.0f}s; "
            f"inspector report at {capture_dir}\n"
        )
    stderr_sink.flush()


def _run_inspection_and_report(
    inspector: Inspector,
    root_pid: int,
    *,
    inspect_dir: Path,
    seq: int,
    elapsed_secs: float,
    stderr_sink,
    final: bool,
) -> Path:
    """Run one capture (background-tick or final-before-kill) and report it.
    Never raises: an inspector failure is itself reported as a warning rather
    than lost, since a broken capture must never take down the cook run it is
    only observing.
    """
    capture_dir = Path(inspect_dir) / capture_dir_name(seq, elapsed_secs)
    try:
        capture_dir.mkdir(parents=True, exist_ok=True)
        inspector(root_pid, capture_dir)
    except Exception as error:  # pylint: disable=broad-except
        stderr_sink.write(
            f"::warning title=soldr cook::inspector capture failed at {capture_dir}: {error}\n"
        )
        stderr_sink.flush()
        return capture_dir

    label = "final capture before hard timeout" if final else f"periodic capture #{seq}"
    _report_capture(
        stderr_sink, capture_dir, label, warn=not final, elapsed_secs=elapsed_secs
    )
    return capture_dir


def stream_and_capture(
    command: list[str],
    cwd: Path,
    timeout_secs: float = DEFAULT_TIMEOUT_SECS,
    *,
    stdout_sink=None,
    stderr_sink=None,
    inspect_every_secs: float = 0.0,
    inspect_dir: Path | None = None,
    inspector: Inspector | None = None,
) -> subprocess.CompletedProcess[str]:
    """Run `command`, streaming both streams live to `stdout_sink`/
    `stderr_sink` (defaulting to the real `sys.stdout`/`sys.stderr`) while
    also capturing them as text, and enforcing `timeout_secs` as a
    wall-clock ceiling with a SIGQUIT-then-SIGTERM shutdown.

    Unlike `subprocess.run(..., capture_output=True)`, the job log shows
    output as `soldr cook` produces it rather than only after the process
    exits (issue: 23-36 minutes of total silence on a slow cook).

    While `command` runs, a NON-FATAL `cook_inspect` capture of its whole
    process tree (plus any soldr daemon/broker processes) fires every
    `inspect_every_secs` seconds when `inspect_every_secs > 0` and
    `inspect_dir` is given -- each one prints its report in a `::group::`
    plus a `::warning` and never kills or fails the run. Each capture runs on
    its own background thread so it never blocks output pumping or the wait
    loop; if a capture is still running when its next tick comes due, that
    tick is skipped rather than queued (`next_tick_number_due` never fires
    twice for ticks missed during one overrun). The existing hard
    `timeout_secs` ceiling is unchanged: on top of the periodic captures, one
    final synchronous capture runs immediately before the SIGQUIT-then-SIGTERM
    shutdown, so the genuinely-wedged case still gets an inspector report,
    not just the pre-existing dump `soldr cook` itself may already have
    written under `SOLDR_COOK_NO_PROGRESS_SECS` (see `cook_inspect.py`'s
    module docstring for how the two relate).
    """
    stdout_sink = stdout_sink if stdout_sink is not None else sys.stdout
    stderr_sink = stderr_sink if stderr_sink is not None else sys.stderr
    inspector = inspector if inspector is not None else default_inspector
    periodic_enabled = inspect_every_secs > 0 and inspect_dir is not None

    with subprocess.Popen(
        command,
        cwd=cwd,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
    ) as proc:
        out_chunks: list[str] = []
        err_chunks: list[str] = []
        out_thread = threading.Thread(
            target=_pump_stream,
            args=(proc.stdout, stdout_sink, out_chunks),
            daemon=True,
        )
        err_thread = threading.Thread(
            target=_pump_stream,
            args=(proc.stderr, stderr_sink, err_chunks),
            daemon=True,
        )
        out_thread.start()
        err_thread.start()

        started = time.monotonic()
        ticks_fired = 0
        capture_seq = 0
        active_capture: dict[str, threading.Thread | None] = {"thread": None}
        capture_lock = threading.Lock()

        def launch_periodic_capture(tick_no: int, elapsed_secs: float) -> None:
            nonlocal capture_seq
            with capture_lock:
                current = active_capture["thread"]
                if current is not None and current.is_alive():
                    stderr_sink.write(
                        f"soldr cook: inspector tick {tick_no} skipped -- "
                        "previous capture still running\n"
                    )
                    stderr_sink.flush()
                    return
                capture_seq += 1
                seq = capture_seq
                thread = threading.Thread(
                    target=_run_inspection_and_report,
                    args=(inspector, proc.pid),
                    kwargs={
                        "inspect_dir": inspect_dir,
                        "seq": seq,
                        "elapsed_secs": elapsed_secs,
                        "stderr_sink": stderr_sink,
                        "final": False,
                    },
                    daemon=True,
                )
                active_capture["thread"] = thread
                thread.start()

        timed_out = False
        while True:
            elapsed = time.monotonic() - started
            remaining_hard = timeout_secs - elapsed
            if remaining_hard <= 0:
                timed_out = True
                break
            wait_for = remaining_hard
            if periodic_enabled:
                next_boundary = (ticks_fired + 1) * inspect_every_secs
                wait_for = min(wait_for, max(0.0, next_boundary - elapsed))
            try:
                proc.wait(timeout=wait_for)
                break
            except subprocess.TimeoutExpired:
                if periodic_enabled:
                    elapsed_now = time.monotonic() - started
                    due = next_tick_number_due(
                        elapsed_now, inspect_every_secs, ticks_fired
                    )
                    if due is not None:
                        ticks_fired = due
                        launch_periodic_capture(due, elapsed_now)
                continue

        if timed_out:
            # The process is still alive here -- capture it live BEFORE
            # sending any signal, then terminate, and only then join the pump
            # threads (their pipes only EOF once the process is actually
            # gone). Reordering this would either capture a process already
            # mid-SIGQUIT-teardown or block the join on a still-running
            # process for no reason.
            final_capture_dir: Path | None = None
            if inspect_dir is not None:
                with capture_lock:
                    in_flight = active_capture["thread"]
                if in_flight is not None:
                    in_flight.join(timeout=5.0)
                capture_seq += 1
                final_capture_dir = _run_inspection_and_report(
                    inspector,
                    proc.pid,
                    inspect_dir=inspect_dir,
                    seq=capture_seq,
                    elapsed_secs=timeout_secs,
                    stderr_sink=stderr_sink,
                    final=True,
                )
            message = (
                f"::error title=soldr cook::cook exceeded its {timeout_secs:.0f}s "
                "wall-clock timeout; sent SIGQUIT then SIGTERM to the process tree"
            )
            if final_capture_dir is not None:
                message += f"; inspector report at {final_capture_dir}"
            stderr_sink.write(message + "\n")
            stderr_sink.flush()
            _terminate_with_grace(proc, TIMEOUT_GRACE_SECS)
            returncode = TIMEOUT_EXIT_CODE
        else:
            returncode = proc.returncode if proc.returncode is not None else -1

        out_thread.join(timeout=TIMEOUT_GRACE_SECS)
        err_thread.join(timeout=TIMEOUT_GRACE_SECS)

    return subprocess.CompletedProcess(
        args=command,
        returncode=returncode,
        stdout="".join(out_chunks),
        stderr="".join(err_chunks),
    )


def build_argv(soldr: str, target: str, chef_args: Sequence[str]) -> list[str]:
    """Build the `soldr cook` argv for the stable tree.

    `soldr cook`'s own parser (`parse_cook_args`, crates/soldr-cli/src/cook.rs)
    recognises only `--release --workspace/--all --keep-recipe --prepare-only
    --cook-only --no-trim --target --profile --recipe-path -p/--package`, and
    REJECTS unknown flags before `--`. Everything cargo-chef itself needs
    (like `--all-targets`) must therefore be forwarded after a literal `--`.

    Never pass `--no-trim`: trimming is what keeps the archive inside the
    2.0 GiB allocation soldr#3043 budgets for it.
    """
    return [soldr, "cook", "--workspace", "--target", target, "--", *chef_args]


def classify(stderr: str) -> tuple[str, str]:
    """Classify a `soldr cook` run from its stderr. Returns (outcome, detail).

    `detail` is empty except for `restore-declined`, where it carries the
    `reason=` field off cook's own decision line.
    """
    if HYDRATE_MARKER in stderr:
        return "hydrated", ""
    if WARM_SKIP_MARKER in stderr:
        return "warm-skip", ""
    if DECISION_SKIP_MARKER in stderr:
        marker = "reason="
        for line in stderr.splitlines():
            if DECISION_SKIP_MARKER not in line:
                continue
            index = line.find(marker)
            if index == -1:
                break
            return "restore-declined", line[index + len(marker) :].strip()
        return "restore-declined", ""
    if UNINDEXED_MARKER in stderr:
        return "built-unindexed", ""
    return "built", ""


def cook_archive_bytes(cache_dir: Path) -> int:
    """Sum `<cache_dir>/cache/cook/**/*.tar.zst`, skipping the `.tmp/` staging dir.

    `cache/cook` is the cook archive root: `cook_cache_dir` is
    `paths.cache.join("cook")`, and `SoldrPaths` sets `cache = root.join("cache")`
    where `root` is `SOLDR_CACHE_DIR`
    (crates/soldr-cache/src/cache_lib/cook_archive.rs line 58,
    crates/soldr-core/src/core/paths.rs line 103). `.tmp/<rand>.tar.zst` is an
    in-flight packer write, not a saved archive, so it does not count.
    """
    cook_dir = cache_dir / "cache" / "cook"
    if not cook_dir.is_dir():
        return 0
    total = 0
    for path in cook_dir.rglob("*.tar.zst"):
        if ".tmp" in path.relative_to(cook_dir).parts:
            continue
        try:
            total += path.stat().st_size
        except OSError:
            continue
    return total


def report_lines(
    target: str,
    outcome: str,
    detail: str,
    elapsed_seconds: float,
    archive_bytes: int | None,
) -> list[str]:
    """Human-readable summary lines, shared between stdout and the step summary."""
    line = f"cook[{target}]: outcome={outcome} elapsed={elapsed_seconds:.1f}s"
    if detail:
        line += f" reason={detail!r}"
    lines = [line]
    if archive_bytes is not None:
        mib = archive_bytes / (1024 * 1024)
        lines.append(
            f"cook[{target}]: cache/cook archive size={mib:.1f} MiB "
            "(counts against soldr#3047's 2.0 GiB allocation)"
        )
    return lines


def append_summary(lines: Sequence[str]) -> None:
    """Append `lines` as a bullet list to `GITHUB_STEP_SUMMARY`, when set."""
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if not summary:
        return
    try:
        with Path(summary).open("a", encoding="utf-8") as handle:
            for line in lines:
                handle.write(f"- {line}\n")
    except OSError as error:
        print(f"run_stable_cook: summary unwritable: {error}", file=sys.stderr)


def main(argv: list[str] | None = None, runner: Runner | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--soldr", required=True, help="absolute path to the soldr binary to run"
    )
    parser.add_argument(
        "--target", required=True, help="target triple, e.g. x86_64-unknown-linux-gnu"
    )
    parser.add_argument(
        "--chef-arg",
        dest="chef_args",
        action="append",
        default=None,
        help=(
            "cargo-chef arg forwarded after `--`; repeatable (default: "
            "--all-targets). A value that itself starts with `-` must use "
            "the `--chef-arg=--flag` form -- argparse cannot otherwise tell "
            "it apart from a new option"
        ),
    )
    parser.add_argument(
        "--require-warm",
        action="store_true",
        help="fail (exit 4) when the run did not hydrate or warm-skip",
    )
    parser.add_argument(
        "--cache-dir",
        default=None,
        help="SOLDR_CACHE_DIR, used only to report the cook archive size",
    )
    parser.add_argument(
        "--timeout-secs",
        type=float,
        default=DEFAULT_TIMEOUT_SECS,
        help=(
            "outer wall-clock ceiling for the whole `soldr cook` invocation "
            f"(default {DEFAULT_TIMEOUT_SECS}s); only used for the default "
            "streaming runner, ignored when --runner is injected (tests)"
        ),
    )
    parser.add_argument(
        "--inspect-every-secs",
        type=float,
        default=DEFAULT_INSPECT_EVERY_SECS,
        help=(
            "run a non-fatal cook_inspect capture on this cadence while cook "
            f"is still running (default {DEFAULT_INSPECT_EVERY_SECS:.0f}s); "
            "0 disables periodic inspection (the final capture immediately "
            "before the hard --timeout-secs kill is unaffected). Only used "
            "for the default streaming runner, ignored when --runner is "
            "injected (tests)"
        ),
    )
    parser.add_argument(
        "--inspect-dir",
        default=None,
        help=(
            "directory for periodic + final cook_inspect captures (default: "
            "$RUNNER_TEMP/cook-inspect, or ./cook-inspect with no "
            "$RUNNER_TEMP). Only used for the default streaming runner, "
            "ignored when --runner is injected (tests)"
        ),
    )
    args = parser.parse_args(argv)

    chef_args = args.chef_args if args.chef_args else list(DEFAULT_CHEF_ARGS)
    command = build_argv(args.soldr, args.target, chef_args)
    repo_root = Path(__file__).resolve().parents[2]
    inspect_dir = Path(args.inspect_dir) if args.inspect_dir else default_inspect_dir()

    # The default (production) path streams live and already wrote every
    # byte to the real stdout/stderr as it arrived, so it must not be
    # echoed again below. An injected `runner=` (always the case in tests)
    # returns a fully-captured result with nothing yet printed, so that path
    # keeps the original capture-then-print behavior.
    already_streamed = runner is None
    started = time.monotonic()
    if runner is None:
        result = stream_and_capture(
            command,
            repo_root,
            args.timeout_secs,
            inspect_every_secs=args.inspect_every_secs,
            inspect_dir=inspect_dir,
        )
    else:
        result = runner(command, repo_root)
    elapsed_seconds = time.monotonic() - started

    if not already_streamed:
        # Capture-then-print (rather than inheriting the parent's streams) so
        # the step log still holds cook's output in full, in order.
        sys.stdout.write(result.stdout)
        sys.stderr.write(result.stderr)

    if result.returncode == COOK_SKIPPED_UNCOOKABLE_WORKSPACE:
        print(
            "::error title=soldr cook::COOK_SKIPPED_UNCOOKABLE_WORKSPACE -- "
            f"cook[{args.target}] was skipped because a path dependency cannot "
            "be materialised by the cargo-chef recipe. Exclude the offending "
            "workspace member with `-p` (soldr#3043 step 3) rather than "
            "relaxing this guard. cook's stderr (echoed above) names the "
            "offending dependency."
        )
        return COOK_SKIPPED_UNCOOKABLE_WORKSPACE
    if result.returncode != 0:
        print(
            f"::error title=soldr cook::cook[{args.target}] exited "
            f"{result.returncode}. Read cook's stderr echoed above. If it "
            f"is cargo-chef rejecting one of the --chef-arg values "
            f"({list(chef_args)!r}), the fix belongs in soldr's argv assembly "
            "(build_chef_cook_args must forward them as bare cargo-chef "
            "options, never after a literal `--`), not in a downgrade flag."
        )
        return result.returncode

    outcome, detail = classify(result.stderr)
    warm = outcome in ("hydrated", "warm-skip")

    if outcome == "built-unindexed":
        print(
            f"::error title=soldr cook::COOK_ARTIFACT_NOT_INDEXED -- cook[{args.target}] "
            "built and packed the archive but its CookRecord found no daemon, so "
            "nothing indexes the artifact and no later run can hydrate from it "
            "(soldr#3117). cook's stderr (echoed above) carries the daemon "
            "error. soldr cook holds the daemon route for its whole run "
            "(cook_route_hold.rs); if that hold failed, its warning is echoed "
            "above too."
        )
    elif warm:
        print(f"::notice title=soldr cook::cook[{args.target}] outcome={outcome}")
    elif args.require_warm:
        print(
            f"::error title=soldr cook::--require-warm set and cook[{args.target}] "
            f"outcome={outcome} (neither hydrated nor warm-skip)"
        )
    else:
        print(
            f"::warning title=soldr cook::cook[{args.target}] outcome={outcome} "
            "(neither hydrated nor warm-skip). Not failing by default -- the "
            "acceptance number is only measurable once soldr#3040's analyzer "
            "lands."
        )

    archive_bytes = cook_archive_bytes(Path(args.cache_dir)) if args.cache_dir else None
    lines = report_lines(args.target, outcome, detail, elapsed_seconds, archive_bytes)
    for line in lines:
        print(line)
    append_summary(lines)

    if outcome == "built-unindexed":
        return COOK_ARTIFACT_NOT_INDEXED
    if args.require_warm and not warm:
        return REQUIRE_WARM_FAILURE
    return 0


if __name__ == "__main__":
    sys.exit(main())
