#!/usr/bin/env python3
"""Measure Cargo's pre-compiler resource fan-out (soldr#2878).

The daemon's compiler-admission gate starts only after Cargo has completed
fingerprinting and directory traversal.  This runner records the resources
used *before and during* a real Cargo-front-door command at a small matrix of
job counts, rather than trying to infer a transient peak from ``docker stats``
after a failure.

It intentionally does not choose a job count or silently retry a failed build:
the command under test remains the source of truth.  The raised-count case is
opt-in because it can reproduce the resource failure being investigated.

Example (run inside the repository's Linux Docker development runner):

  python3 ci/cargo_orchestration_telemetry.py \\
      --raised-jobs 8 --allow-raised-count \\
      --output /tmp/soldr-2878-telemetry.json -- \\
      soldr cargo check -p soldr-cli

The JSON has one row each for N=1, N=2, and the requested raised count.  Every
row includes wall time, cgroup memory/PID/event readings, and the maxima of
observed Cargo/compiler/toolchain processes.  ``max_memory_current_bytes`` is
the per-invocation transient peak; ``memory.peak`` is retained as before/after
context because a read-only cgroup may not allow it to be reset.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import time
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Callable, Iterable


CGROUP_ROOT = Path("/sys/fs/cgroup")
DEFAULT_INTERVAL_SECONDS = 0.2
DEFAULT_TIMEOUT_SECONDS = 30 * 60
TOOLCHAIN_EXECUTABLES = frozenset(
    {
        "cargo",
        "rustc",
        "rustdoc",
        "clippy-driver",
        "rustfmt",
        "cc",
        "c++",
        "clang",
        "clang++",
        "gcc",
        "g++",
    }
)


@dataclass(frozen=True)
class ProcessCounts:
    """A cgroup-wide point-in-time process census.

    The cgroup can include runner helpers, so this counts command names rather
    than presenting ``pids.current`` as if it were exclusively Cargo's.
    """

    cargo: int
    compiler: int
    soldr: int
    toolchain: int


@dataclass(frozen=True)
class Snapshot:
    monotonic_seconds: float
    memory_current_bytes: int | None
    memory_peak_bytes: int | None
    memory_swap_current_bytes: int | None
    pids_current: int | None
    memory_events: dict[str, int]
    processes: ProcessCounts


def _read_text(path: Path) -> str | None:
    try:
        return path.read_text(encoding="utf-8").strip()
    except OSError:
        return None


def _read_int(path: Path) -> int | None:
    raw = _read_text(path)
    if raw is None:
        return None
    try:
        return int(raw)
    except ValueError:
        return None


def _read_memory_events(cgroup_root: Path) -> dict[str, int]:
    raw = _read_text(cgroup_root / "memory.events")
    if raw is None:
        return {}
    events: dict[str, int] = {}
    for line in raw.splitlines():
        fields = line.split()
        if len(fields) != 2:
            continue
        try:
            events[fields[0]] = int(fields[1])
        except ValueError:
            continue
    return events


def _command_name(proc_dir: Path) -> str | None:
    raw = _read_text(proc_dir / "comm")
    if raw:
        return Path(raw).name
    try:
        command = (proc_dir / "cmdline").read_bytes().split(b"\0", 1)[0]
    except OSError:
        return None
    return Path(os.fsdecode(command)).name if command else None


def process_counts(proc_root: Path = Path("/proc")) -> ProcessCounts:
    """Count the active Cargo/compiler/Soldr processes visible in ``/proc``."""
    cargo = compiler = soldr = 0
    try:
        proc_entries: Iterable[Path] = proc_root.iterdir()
        for entry in proc_entries:
            if not entry.name.isdecimal():
                continue
            name = _command_name(entry)
            if name is None:
                continue
            if name == "cargo":
                cargo += 1
            elif name in TOOLCHAIN_EXECUTABLES:
                compiler += 1
            elif name.startswith("soldr"):
                soldr += 1
    except OSError:
        pass
    return ProcessCounts(
        cargo=cargo,
        compiler=compiler,
        soldr=soldr,
        toolchain=cargo + compiler + soldr,
    )


def snapshot(
    cgroup_root: Path = CGROUP_ROOT,
    proc_root: Path = Path("/proc"),
    clock: Callable[[], float] = time.monotonic,
) -> Snapshot:
    """Capture one cheap cgroup/process sample without requiring root access."""
    return Snapshot(
        monotonic_seconds=clock(),
        memory_current_bytes=_read_int(cgroup_root / "memory.current"),
        memory_peak_bytes=_read_int(cgroup_root / "memory.peak"),
        memory_swap_current_bytes=_read_int(cgroup_root / "memory.swap.current"),
        pids_current=_read_int(cgroup_root / "pids.current"),
        memory_events=_read_memory_events(cgroup_root),
        processes=process_counts(proc_root),
    )


def _max_or_none(values: Iterable[int | None]) -> int | None:
    numeric = [value for value in values if value is not None]
    return max(numeric) if numeric else None


def _event_deltas(before: dict[str, int], after: dict[str, int]) -> dict[str, int]:
    return {
        name: value - before.get(name, 0)
        for name, value in after.items()
        if value != before.get(name, 0)
    }


def summarize_samples(samples: list[Snapshot]) -> dict[str, int | None]:
    """Return per-invocation maxima, never confusing them with cgroup lifetime peak."""
    return {
        "max_memory_current_bytes": _max_or_none(
            sample.memory_current_bytes for sample in samples
        ),
        "max_memory_peak_bytes": _max_or_none(sample.memory_peak_bytes for sample in samples),
        "max_memory_swap_current_bytes": _max_or_none(
            sample.memory_swap_current_bytes for sample in samples
        ),
        "max_pids_current": _max_or_none(sample.pids_current for sample in samples),
        "max_cargo_processes": max(sample.processes.cargo for sample in samples),
        "max_compiler_processes": max(sample.processes.compiler for sample in samples),
        "max_soldr_processes": max(sample.processes.soldr for sample in samples),
        "max_toolchain_processes": max(sample.processes.toolchain for sample in samples),
    }


def run_case(
    jobs: int,
    command: list[str],
    *,
    cgroup_root: Path = CGROUP_ROOT,
    proc_root: Path = Path("/proc"),
    interval_seconds: float = DEFAULT_INTERVAL_SECONDS,
    timeout_seconds: float = DEFAULT_TIMEOUT_SECONDS,
    run: Callable[..., subprocess.Popen[bytes]] = subprocess.Popen,
    sleep: Callable[[float], None] = time.sleep,
    clock: Callable[[], float] = time.monotonic,
) -> dict[str, object]:
    """Run one explicit fan-out value and collect samples through process exit."""
    if jobs < 1:
        raise ValueError("jobs must be positive")
    environment = os.environ.copy()
    environment["CARGO_BUILD_JOBS"] = str(jobs)
    environment["SOLDR_JOBS"] = str(jobs)
    environment["SOLDR_CI_ORCHESTRATION_TELEMETRY_JOBS"] = str(jobs)
    started = snapshot(cgroup_root, proc_root, clock)
    samples = [started]
    began = clock()
    process = run(command, env=environment)
    timed_out = False
    while process.poll() is None:
        if clock() - began >= timeout_seconds:
            timed_out = True
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
            break
        sleep(interval_seconds)
        samples.append(snapshot(cgroup_root, proc_root, clock))
    if not timed_out:
        process.wait()
    finished = snapshot(cgroup_root, proc_root, clock)
    samples.append(finished)
    return {
        "requested_jobs": jobs,
        "returncode": process.returncode,
        "timed_out": timed_out,
        "wall_time_ms": round((finished.monotonic_seconds - started.monotonic_seconds) * 1000),
        "start": asdict(started),
        "end": asdict(finished),
        "memory_events_delta": _event_deltas(started.memory_events, finished.memory_events),
        "maxima": summarize_samples(samples),
        "samples": len(samples),
    }


def parse_jobs(raw: str) -> list[int]:
    values: list[int] = []
    for item in raw.split(","):
        try:
            value = int(item.strip())
        except ValueError as error:
            raise argparse.ArgumentTypeError(f"invalid job count: {item!r}") from error
        if value < 1:
            raise argparse.ArgumentTypeError("job counts must be positive")
        if value not in values:
            values.append(value)
    if not values:
        raise argparse.ArgumentTypeError("at least one job count is required")
    return values


def format_markdown(results: list[dict[str, object]]) -> str:
    """Compact human rendering; JSON remains the authoritative evidence."""
    lines = [
        "| jobs | result | wall | max current | max PIDs | max Cargo/compiler/tools | OOM delta |",
        "| ---: | :--- | ---: | ---: | ---: | ---: | ---: |",
    ]
    for result in results:
        maxima = result["maxima"]
        assert isinstance(maxima, dict)
        events = result["memory_events_delta"]
        assert isinstance(events, dict)
        outcome = "timeout" if result["timed_out"] else str(result["returncode"])
        lines.append(
            "| {jobs} | {outcome} | {wall} ms | {memory} | {pids} | {cargo}/{compiler}/{tools} | {oom} |".format(
                jobs=result["requested_jobs"],
                outcome=outcome,
                wall=result["wall_time_ms"],
                memory=maxima["max_memory_current_bytes"],
                pids=maxima["max_pids_current"],
                cargo=maxima["max_cargo_processes"],
                compiler=maxima["max_compiler_processes"],
                tools=maxima["max_toolchain_processes"],
                oom=events.get("oom_kill", 0) + events.get("oom_group_kill", 0),
            )
        )
    return "\n".join(lines)


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--jobs",
        type=parse_jobs,
        default=[1, 2],
        help="safe baseline job counts (default: 1,2)",
    )
    parser.add_argument(
        "--raised-jobs",
        type=int,
        help="formerly failing/high fan-out count; requires --allow-raised-count",
    )
    parser.add_argument(
        "--allow-raised-count",
        action="store_true",
        help="acknowledge that the raised count may exhaust the constrained host",
    )
    parser.add_argument("--interval-seconds", type=float, default=DEFAULT_INTERVAL_SECONDS)
    parser.add_argument("--timeout-seconds", type=float, default=DEFAULT_TIMEOUT_SECONDS)
    parser.add_argument("--output", type=Path, help="write JSON evidence to this path")
    parser.add_argument("command", nargs=argparse.REMAINDER, help="command prefixed by --")
    parsed = parser.parse_args(argv)
    if parsed.raised_jobs is not None:
        if parsed.raised_jobs < 1:
            parser.error("--raised-jobs must be positive")
        if not parsed.allow_raised_count:
            parser.error("--raised-jobs requires --allow-raised-count")
        if parsed.raised_jobs not in parsed.jobs:
            parsed.jobs.append(parsed.raised_jobs)
    if parsed.interval_seconds <= 0:
        parser.error("--interval-seconds must be positive")
    if parsed.timeout_seconds <= 0:
        parser.error("--timeout-seconds must be positive")
    if parsed.command[:1] == ["--"]:
        parsed.command = parsed.command[1:]
    if not parsed.command:
        parser.error("provide a command after --")
    return parsed


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    results = [
        run_case(
            jobs,
            args.command,
            interval_seconds=args.interval_seconds,
            timeout_seconds=args.timeout_seconds,
        )
        for jobs in args.jobs
    ]
    report = {
        "schema_version": 1,
        "purpose": "soldr#2878 Cargo pre-compiler orchestration telemetry",
        "cgroup_root": str(CGROUP_ROOT),
        "command": args.command,
        "results": results,
        "notes": [
            "max_memory_current_bytes is sampled for this invocation.",
            "memory.peak is a cgroup lifetime high-water mark unless the host reset it.",
            "Raised counts are explicit reproduction inputs; Soldr intentionally preserves explicit CARGO_BUILD_JOBS.",
        ],
    }
    rendered = json.dumps(report, indent=2, sort_keys=True) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(rendered, encoding="utf-8")
        print(f"wrote telemetry evidence: {args.output}", file=sys.stderr)
    else:
        print(rendered)
    print(format_markdown(results), file=sys.stderr)
    return 0 if all(result["returncode"] == 0 and not result["timed_out"] for result in results) else 1


if __name__ == "__main__":
    raise SystemExit(main())
