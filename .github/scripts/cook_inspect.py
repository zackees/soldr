#!/usr/bin/env python3
"""Best-effort on-CPU / off-CPU stack inspector for a stalled process tree
(soldr#3043 cook-timeout follow-up).

`run_stable_cook.py` drives `soldr cook` in CI and, historically, only ever
saw a wedged run as 23-37 minutes of total silence followed by an outer
wall-clock timeout with no idea *where* soldr was stuck. This module is the
diagnostic layer that gets pointed at the running cook process tree (and any
discoverable `soldr` daemon/broker processes, which are NOT descendants of
cook and must be found by name/cmdline the same way
`crates/soldr-cli/src/cook_watchdog_dump.rs::soldr_service_processes` does)
and captures, best-effort:

  1. a process table (pid, ppid, state, wchan, elapsed, cpu time, rss,
     cmdline) for the whole tree plus the soldr service processes;
  2. per-thread kernel stacks + wchan straight from `/proc` -- the cheap
     "where is it blocked" off-CPU view, read with `sudo -n` when the direct
     read is refused (GitHub-hosted runners carry passwordless sudo);
  3. on-CPU sampling via `perf record -F 99 -g`, folded into the top stacks
     by sample count;
  4. off-CPU sampling via `perf record -e sched:sched_switch -g` (or, when
     present, `offcputime-bpfcc`, which reports actual off-CPU durations
     rather than switch-event counts and is preferred when available);
  5. symbolized all-thread backtraces via `gdb -batch -p <pid> -ex "thread
     apply all bt"` (falling back to `eu-stack -p <pid>` when gdb is
     missing).

Every step is wrapped so a missing tool, a permission refusal, or an
unexpected error degrades *that* step's output rather than raising -- a
partial capture is far more useful than an exception that discards
everything gathered so far (same policy as
`cook_watchdog_dump::write_stall_dump` on the Rust side).

Two files tie the capture together: `SUMMARY.md` is a short human summary
(counts, which sampling succeeded, pointers to the raw files) and
`REPORT.txt` concatenates the summary with the full backtraces, thread
stacks, and folded on-/off-CPU stacks -- sized to be printed straight into a
CI log group. The individual raw files (`process-table.txt`,
`thread-stacks.txt`, `oncpu-*`, `offcpu-*`, `backtraces/backtrace-<pid>.txt`)
remain on disk for the artifact upload.

This module is deliberately NOT a reimplementation of, or a replacement for,
`crates/soldr-cli/src/cook_watchdog.rs` / `cook_watchdog_dump.rs`. That Rust
watchdog runs *inside* the `soldr cook` process itself, fires on
`SOLDR_COOK_NO_PROGRESS_SECS` (default 900s) genuine no-progress, and writes
its own lighter dump (a process list plus a `gdb`/`eu-stack` backtrace per
descendant) before failing the run outright -- it cannot see the daemon
process from outside and it has no perf sampling. This module runs from
*outside* the process, from the CI wrapper (`run_stable_cook.py`), on a
periodic non-fatal cadence regardless of whether cook itself is making
progress, and adds the on-CPU/off-CPU perf views and the daemon/broker
process discovery the in-process watchdog cannot do. The two are
complementary: if the in-process watchdog also fires, its dump lands under
`$SOLDR_CACHE_DIR/logs/cook-stall-<timestamp>/` independently of whatever
this module already wrote to `--inspect-dir`.

Standalone CLI usage::

    cook_inspect.py --root-pid 12345 --out /tmp/cook-inspect-1 --sample-secs 30

`run_stable_cook.py` imports `run_inspection` directly (not as a subprocess)
so its periodic and final captures share this process's already-elevated
`sudo -n` state and do not pay a second Python interpreter startup.

Owner rule: this module never swallows a child process's stdout/stderr.
Every `perf` / `gdb` / `eu-stack` / `offcputime-bpfcc` / `sudo` / `sysctl`
invocation has its stderr forwarded live to this process's stderr (prefixed
with the tool label) and both streams are also saved under
`<out_dir>/proc-logs/<label>.std{out,err}.log` -- `_run_best_effort` is the
one place that runs a child process, and it is the one place that does this,
so no call site can silently discard a stream.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import dataclasses
import os
import shutil
import subprocess
import sys
import threading
import time
from pathlib import Path

# on-CPU and off-CPU sampling run concurrently (each in its own thread) and
# both forward stderr through `_forward_and_log`; this serializes the writes
# so two tools' stderr lines never interleave mid-line in the job log.
_STDERR_LOCK = threading.Lock()

DEFAULT_SAMPLE_SECS = 30.0
# Bound on any single native backtrace tool invocation (one process).
BACKTRACE_TIMEOUT_SECS = 20.0
# Extra slack added to `perf record -- sleep <sample_secs>`'s own timeout so
# perf's process teardown (writing perf.data) is never cut off right at the
# sampling boundary.
PERF_TIMEOUT_SLACK_SECS = 30.0
# Overall soft cap on one `run_inspection()` call (owner request: "total
# inspector runtime cap ~3 min"). Checked between phases, not preemptively --
# a phase already running is let finish rather than killed mid-capture.
INSPECTOR_BUDGET_SECS = 180.0
# How many folded stacks (on-CPU and off-CPU each) to keep.
TOP_STACKS = 40
# Linux's near-universal USER_HZ. `sysconf(_SC_CLK_TCK)` is the correct way
# to get this, but every mainstream Linux distribution (including every
# GitHub-hosted runner image) fixes it at 100, and a hard-coded constant
# keeps `format_process_table`'s elapsed/cpu-time math trivially testable
# against a fixed fixture without also mocking `os.sysconf`.
CLK_TCK = 100


@dataclasses.dataclass(frozen=True)
class ProcEntry:
    """One `/proc/<pid>` snapshot."""

    pid: int
    ppid: int
    state: str
    comm: str
    cmdline: str
    rss_kb: int
    wchan: str
    utime_ticks: int
    stime_ticks: int
    starttime_ticks: int


# ---------------------------------------------------------------------------
# /proc reading (fixture-injectable via `proc_root`).
# ---------------------------------------------------------------------------


def _read_text(path: Path) -> str | None:
    try:
        return path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None


def list_proc_pids(proc_root: Path) -> list[int]:
    """Numeric entries directly under `proc_root` (i.e. `/proc/<pid>`)."""
    try:
        entries = os.listdir(proc_root)
    except OSError:
        return []
    return sorted(int(name) for name in entries if name.isdigit())


def _parse_stat(text: str) -> tuple[str, int, str, int, int, int] | None:
    """Parse the fields `read_proc_entry` needs out of `/proc/<pid>/stat`.

    Field 2 (`comm`) is parenthesized and may itself contain spaces or
    parens, so it is located by the outermost paren pair rather than a naive
    whitespace split; every field after it is positional from there.
    """
    first_paren = text.find("(")
    last_paren = text.rfind(")")
    if first_paren == -1 or last_paren == -1 or last_paren < first_paren:
        return None
    comm = text[first_paren + 1 : last_paren]
    rest = text[last_paren + 2 :].split()
    # state(0) ppid(1) ... utime(11) stime(12) ... starttime(19), 0-indexed
    # from `rest[0]` == stat field 3.
    if len(rest) < 20:
        return None
    try:
        state = rest[0]
        ppid = int(rest[1])
        utime = int(rest[11])
        stime = int(rest[12])
        starttime = int(rest[19])
    except ValueError:
        return None
    return comm, ppid, state, utime, stime, starttime


def read_proc_entry(proc_root: Path, pid: int) -> ProcEntry | None:
    """Best-effort `ProcEntry` for one pid; `None` if it already exited."""
    stat_text = _read_text(proc_root / str(pid) / "stat")
    if stat_text is None:
        return None
    parsed = _parse_stat(stat_text)
    if parsed is None:
        return None
    comm, ppid, state, utime, stime, starttime = parsed

    cmdline_raw = _read_text(proc_root / str(pid) / "cmdline") or ""
    cmdline = " ".join(part for part in cmdline_raw.split("\x00") if part)

    wchan = (_read_text(proc_root / str(pid) / "wchan") or "").strip()

    rss_kb = 0
    status_text = _read_text(proc_root / str(pid) / "status") or ""
    for line in status_text.splitlines():
        if line.startswith("VmRSS:"):
            digits = "".join(ch for ch in line if ch.isdigit())
            rss_kb = int(digits) if digits else 0
            break

    return ProcEntry(
        pid=pid,
        ppid=ppid,
        state=state,
        comm=comm,
        cmdline=cmdline or f"[{comm}]",
        rss_kb=rss_kb,
        wchan=wchan,
        utime_ticks=utime,
        stime_ticks=stime,
        starttime_ticks=starttime,
    )


def build_process_table(proc_root: Path) -> dict[int, ProcEntry]:
    table: dict[int, ProcEntry] = {}
    for pid in list_proc_pids(proc_root):
        entry = read_proc_entry(proc_root, pid)
        if entry is not None:
            table[pid] = entry
    return table


def descendants_of(table: dict[int, ProcEntry], root_pid: int) -> list[ProcEntry]:
    """BFS over `ppid` links, mirroring `cook_watchdog_dump.rs::descendants_of`."""
    result: list[ProcEntry] = []
    seen = {root_pid}
    frontier = [root_pid]
    while frontier:
        parent = frontier.pop()
        for entry in table.values():
            if entry.ppid == parent and entry.pid not in seen:
                seen.add(entry.pid)
                result.append(entry)
                frontier.append(entry.pid)
    return result


def soldr_service_entries(table: dict[int, ProcEntry]) -> list[ProcEntry]:
    """The daemon/broker are not descendants of cook, so find them by
    name/cmdline -- mirrors `cook_watchdog_dump.rs::soldr_service_processes`.
    """
    result = []
    for entry in table.values():
        if not entry.comm.startswith("soldr"):
            continue
        if "daemon" in entry.cmdline or "broker" in entry.cmdline:
            result.append(entry)
    return result


def _read_uptime_ticks(proc_root: Path) -> int | None:
    text = _read_text(proc_root / "uptime")
    if not text:
        return None
    try:
        seconds = float(text.split()[0])
    except (ValueError, IndexError):
        return None
    return int(seconds * CLK_TCK)


def format_process_table(
    entries: list[ProcEntry], *, uptime_ticks: int | None = None
) -> str:
    if not entries:
        return "(none found)\n"
    lines = []
    for entry in sorted(entries, key=lambda item: item.pid):
        cpu_secs = (entry.utime_ticks + entry.stime_ticks) / CLK_TCK
        elapsed = "?"
        if uptime_ticks is not None:
            elapsed = f"{max(0, uptime_ticks - entry.starttime_ticks) / CLK_TCK:.0f}"
        lines.append(
            f"pid={entry.pid} ppid={entry.ppid} state={entry.state} "
            f"wchan={entry.wchan or '-'} elapsed={elapsed}s "
            f"cpu_time={cpu_secs:.1f}s rss_kb={entry.rss_kb} cmd={entry.cmdline}"
        )
    return "\n".join(lines) + "\n"


# ---------------------------------------------------------------------------
# Best-effort subprocess helper. Every native-tool call in this module goes
# through here so "tool missing / refused / timed out" is one code path.
# ---------------------------------------------------------------------------


def maybe_sudo(argv: list[str]) -> list[str]:
    """Prefix with `sudo -n` unless already root. `-n` (non-interactive)
    means a runner without passwordless sudo fails the command instead of
    hanging on a password prompt -- exactly the "never block" contract this
    whole module holds to.
    """
    if hasattr(os, "geteuid") and os.geteuid() == 0:
        return argv
    return ["sudo", "-n", *argv]


def _log_dir(out_dir: Path) -> Path:
    path = out_dir / "proc-logs"
    path.mkdir(parents=True, exist_ok=True)
    return path


def _forward_and_log(
    label: str,
    argv: list[str],
    stdout_text: str,
    stderr_text: str,
    *,
    out_dir: Path | None,
    stderr_sink,
) -> None:
    """The one place a child's streams are disposed of. stdout is returned to
    the caller by `_run_best_effort` for parsing; this only handles the
    "never swallow" side: stderr is forwarded live (prefixed with `label`)
    and both streams are archived to `<out_dir>/proc-logs/` when an `out_dir`
    is given (it is omitted only for the tiny sysctl/relax calls that have no
    capture directory yet).
    """
    with _STDERR_LOCK:
        if stderr_text:
            for line in stderr_text.splitlines():
                stderr_sink.write(f"[{label}] {line}\n")
            stderr_sink.flush()
    if out_dir is None:
        return
    logs = _log_dir(out_dir)
    cmd_line = " ".join(argv) + "\n"
    (logs / f"{label}.stdout.log").write_text(cmd_line + stdout_text, encoding="utf-8")
    (logs / f"{label}.stderr.log").write_text(cmd_line + stderr_text, encoding="utf-8")


def _coerce_text(value: bytes | str | None) -> str:
    """Normalize `subprocess.TimeoutExpired`'s `bytes | str | None`-typed
    `stdout`/`stderr` to `str`. Runtime always sees `str | None` here (this
    module only ever calls `subprocess.run(..., text=True)`); this exists so
    mypy does not have to trust that invariant.
    """
    if value is None:
        return ""
    if isinstance(value, bytes):
        return value.decode("utf-8", "replace")
    return value


def _run_best_effort(
    argv: list[str],
    *,
    timeout: float = 10.0,
    label: str = "cmd",
    out_dir: Path | None = None,
    stderr_sink=None,
) -> str | None:
    """Run `argv`, returning stdout text or `None` on any failure (missing
    binary, non-zero exit with no stdout, or a timeout). Never raises.

    stdout and stderr are both captured (never `DEVNULL`): stdout is this
    function's return value, and stderr is forwarded to `stderr_sink`
    (default `sys.stderr`) plus archived alongside stdout under
    `<out_dir>/proc-logs/<label>.std{out,err}.log` -- see `_forward_and_log`.
    A timeout still has partial output on the `TimeoutExpired` exception
    itself, which is captured and forwarded the same way as a normal
    completion, since a slow perf/gdb invocation's stderr up to the timeout
    is exactly the detail a forensic capture needs.
    """
    sink = stderr_sink if stderr_sink is not None else sys.stderr
    try:
        result = subprocess.run(
            argv,
            capture_output=True,
            text=True,
            timeout=timeout,
            check=False,
        )
        stdout_text, stderr_text, returncode = (
            result.stdout,
            result.stderr,
            result.returncode,
        )
    except subprocess.TimeoutExpired as error:
        # `text=True` was passed to the `subprocess.run` above, so a timeout's
        # partial output is already `str | None` at runtime -- `_coerce_text`
        # only exists to satisfy `TimeoutExpired`'s generic `bytes | str`
        # typing, not because `bytes` is actually expected here.
        stdout_text = _coerce_text(error.stdout)
        stderr_text = (
            _coerce_text(error.stderr) + f"\n[{label}] timed out after {timeout}s\n"
        )
        _forward_and_log(
            label, argv, stdout_text, stderr_text, out_dir=out_dir, stderr_sink=sink
        )
        return None
    except OSError as error:
        _forward_and_log(
            label, argv, "", f"{error}\n", out_dir=out_dir, stderr_sink=sink
        )
        return None

    _forward_and_log(
        label, argv, stdout_text, stderr_text, out_dir=out_dir, stderr_sink=sink
    )
    if returncode != 0 and not stdout_text:
        return None
    return stdout_text


def relax_perf_restrictions_best_effort(
    *, out_dir: Path | None = None, stderr_sink=None
) -> None:
    """`sudo -n sysctl -w kernel.perf_event_paranoid=-1` and
    `kernel.yama.ptrace_scope=0`, best-effort. GitHub-hosted Linux runners
    carry passwordless sudo; a host that refuses just keeps its existing
    (possibly more restrictive) settings and the later capture steps degrade
    accordingly.
    """
    for name, value in (
        ("kernel.perf_event_paranoid", "-1"),
        ("kernel.yama.ptrace_scope", "0"),
    ):
        _run_best_effort(
            maybe_sudo(["sysctl", "-w", f"{name}={value}"]),
            timeout=5.0,
            label=f"sysctl-{name.replace('.', '_')}",
            out_dir=out_dir,
            stderr_sink=stderr_sink,
        )


# ---------------------------------------------------------------------------
# Off-CPU: per-thread kernel stacks straight from /proc.
# ---------------------------------------------------------------------------


def list_task_ids(proc_root: Path, pid: int) -> list[int]:
    try:
        entries = os.listdir(proc_root / str(pid) / "task")
    except OSError:
        return []
    return sorted(int(name) for name in entries if name.isdigit())


def read_thread_stack(
    proc_root: Path,
    pid: int,
    tid: int,
    *,
    use_sudo: bool,
    out_dir: Path | None = None,
    stderr_sink=None,
) -> tuple[str, str]:
    """`(kernel stack, wchan)` for one thread, falling back to `sudo -n cat`
    when the direct read is refused (ptrace_scope, or a differently-owned
    daemon/broker process).
    """
    stack_path = proc_root / str(pid) / "task" / str(tid) / "stack"
    wchan_path = proc_root / str(pid) / "task" / str(tid) / "wchan"
    stack = _read_text(stack_path)
    if stack is None and use_sudo:
        stack = _run_best_effort(
            ["sudo", "-n", "cat", str(stack_path)],
            label=f"sudo-cat-stack-{pid}-{tid}",
            out_dir=out_dir,
            stderr_sink=stderr_sink,
        )
    wchan = _read_text(wchan_path)
    if wchan is None and use_sudo:
        wchan = _run_best_effort(
            ["sudo", "-n", "cat", str(wchan_path)],
            label=f"sudo-cat-wchan-{pid}-{tid}",
            out_dir=out_dir,
            stderr_sink=stderr_sink,
        )
    return (stack or "(unreadable)"), ((wchan or "").strip() or "(unreadable)")


def capture_thread_stacks(
    proc_root: Path,
    pids: list[int],
    *,
    use_sudo: bool,
    out_dir: Path | None = None,
    stderr_sink=None,
) -> str:
    lines = []
    for pid in pids:
        tids = list_task_ids(proc_root, pid)
        if not tids:
            lines.append(f"pid={pid}: no task/ entries (already exited?)")
            continue
        for tid in tids:
            stack, wchan = read_thread_stack(
                proc_root,
                pid,
                tid,
                use_sudo=use_sudo,
                out_dir=out_dir,
                stderr_sink=stderr_sink,
            )
            lines.append(f"pid={pid} tid={tid} wchan={wchan}\n{stack.rstrip()}")
    return ("\n\n".join(lines) + "\n") if lines else "(no threads found)\n"


# ---------------------------------------------------------------------------
# perf-script folding (pure, fixture-testable).
# ---------------------------------------------------------------------------


def fold_perf_script(text: str, top_n: int = TOP_STACKS) -> list[tuple[str, int]]:
    """Fold `perf script` output into (stack, sample_count) pairs, sorted by
    count descending, top `top_n` kept.

    `perf script`'s text format is one blank-line-terminated stanza per
    sample: a non-indented header line (comm/pid/cpu/timestamp), then one
    indented frame line per stack frame, innermost first. Frames are folded
    leaf-to-root reversed (root first) into a single `;`-joined stack string,
    collapsing the address and module-file so identical logical stacks merge
    even when `perf script` prints slightly different literal text (e.g. a
    module offset) around the same symbol.
    """
    counts: dict[str, int] = {}
    order: list[str] = []
    frames: list[str] = []

    def flush() -> None:
        if not frames:
            return
        stack = ";".join(reversed(frames))
        if stack not in counts:
            order.append(stack)
        counts[stack] = counts.get(stack, 0) + 1
        frames.clear()

    for raw_line in text.splitlines():
        line = raw_line.rstrip("\n")
        if not line.strip():
            flush()
            continue
        if not line[:1].isspace():
            # A new sample's header line; any frames collected under the
            # previous header (if this stanza had none) are flushed first.
            flush()
            continue
        stripped = line.strip()
        parts = stripped.split(None, 1)
        frame = parts[1] if len(parts) > 1 else stripped
        if " (" in frame:
            frame = frame.rsplit(" (", 1)[0]
        frames.append(frame.strip())
    flush()

    ranked = sorted(order, key=lambda stack: counts[stack], reverse=True)
    return [(stack, counts[stack]) for stack in ranked[:top_n]]


def render_folded_stacks(folded: list[tuple[str, int]]) -> str:
    if not folded:
        return "(no samples)\n"
    return "\n".join(f"{count:6d}  {stack}" for stack, count in folded) + "\n"


# ---------------------------------------------------------------------------
# On-CPU / off-CPU sampling via perf (or offcputime-bpfcc for off-CPU).
# ---------------------------------------------------------------------------


def capture_on_cpu(
    pids: list[int], out_dir: Path, sample_secs: float, *, stderr_sink=None
) -> str:
    if not pids:
        return "no pids to sample"
    perf = shutil.which("perf")
    if perf is None:
        return "perf not found on PATH"
    data_path = out_dir / "oncpu-perf.data"
    pid_list = ",".join(str(pid) for pid in pids)
    argv = maybe_sudo(
        [
            perf,
            "record",
            "-F",
            "99",
            "-g",
            "-o",
            str(data_path),
            "-p",
            pid_list,
            "--",
            "sleep",
            str(max(1, int(sample_secs))),
        ]
    )
    _run_best_effort(
        argv,
        timeout=sample_secs + PERF_TIMEOUT_SLACK_SECS,
        label="perf-record-oncpu",
        out_dir=out_dir,
        stderr_sink=stderr_sink,
    )
    if not data_path.exists():
        return "perf record produced no data (permission denied, or every pid already exited)"
    script = _run_best_effort(
        maybe_sudo([perf, "script", "-i", str(data_path)]),
        timeout=60.0,
        label="perf-script-oncpu",
        out_dir=out_dir,
        stderr_sink=stderr_sink,
    )
    if not script:
        return "perf record succeeded but `perf script` produced no output"
    (out_dir / "oncpu-script.txt").write_text(script, encoding="utf-8")
    folded = fold_perf_script(script)
    (out_dir / "oncpu-summary.txt").write_text(
        render_folded_stacks(folded), encoding="utf-8"
    )
    return f"{len(folded)} distinct on-CPU stacks captured (top {TOP_STACKS} in oncpu-summary.txt)"


def capture_off_cpu(
    pids: list[int], out_dir: Path, sample_secs: float, *, stderr_sink=None
) -> str:
    bpfcc = shutil.which("offcputime-bpfcc")
    if bpfcc is not None:
        return _capture_off_cpu_bpfcc(
            bpfcc, pids, out_dir, sample_secs, stderr_sink=stderr_sink
        )
    return _capture_off_cpu_perf(pids, out_dir, sample_secs, stderr_sink=stderr_sink)


def _capture_off_cpu_bpfcc(
    bpfcc: str,
    pids: list[int],
    out_dir: Path,
    sample_secs: float,
    *,
    stderr_sink=None,
) -> str:
    """`offcputime-bpfcc` reports actual off-CPU *durations* per stack,
    strictly more informative than a sched_switch event count, so it is
    preferred whenever present.
    """
    if not pids:
        return "no pids to sample"
    captured = []
    for pid in pids:
        output = _run_best_effort(
            maybe_sudo([bpfcc, "-p", str(pid), "-f", str(max(1, int(sample_secs)))]),
            timeout=sample_secs + PERF_TIMEOUT_SLACK_SECS,
            label=f"offcputime-bpfcc-{pid}",
            out_dir=out_dir,
            stderr_sink=stderr_sink,
        )
        if output:
            (out_dir / f"offcpu-bpfcc-{pid}.txt").write_text(output, encoding="utf-8")
            captured.append(pid)
    if not captured:
        return "offcputime-bpfcc found but produced no output for any pid"
    combined = "\n".join(
        (out_dir / f"offcpu-bpfcc-{pid}.txt").read_text(encoding="utf-8")
        for pid in captured
    )
    (out_dir / "offcpu-summary.txt").write_text(combined, encoding="utf-8")
    return f"offcputime-bpfcc captured for pids {captured}"


def _capture_off_cpu_perf(
    pids: list[int], out_dir: Path, sample_secs: float, *, stderr_sink=None
) -> str:
    if not pids:
        return "no pids to sample"
    perf = shutil.which("perf")
    if perf is None:
        return "neither offcputime-bpfcc nor perf found on PATH"
    data_path = out_dir / "offcpu-perf.data"
    pid_list = ",".join(str(pid) for pid in pids)
    argv = maybe_sudo(
        [
            perf,
            "record",
            "-e",
            "sched:sched_switch",
            "-g",
            "-o",
            str(data_path),
            "-p",
            pid_list,
            "--",
            "sleep",
            str(max(1, int(sample_secs))),
        ]
    )
    _run_best_effort(
        argv,
        timeout=sample_secs + PERF_TIMEOUT_SLACK_SECS,
        label="perf-record-offcpu",
        out_dir=out_dir,
        stderr_sink=stderr_sink,
    )
    if not data_path.exists():
        return "perf sched_switch record produced no data (permission denied, or every pid already exited)"
    script = _run_best_effort(
        maybe_sudo([perf, "script", "-i", str(data_path)]),
        timeout=60.0,
        label="perf-script-offcpu",
        out_dir=out_dir,
        stderr_sink=stderr_sink,
    )
    if not script:
        return "perf record succeeded but `perf script` produced no output"
    (out_dir / "offcpu-script.txt").write_text(script, encoding="utf-8")
    folded = fold_perf_script(script)
    (out_dir / "offcpu-summary.txt").write_text(
        render_folded_stacks(folded), encoding="utf-8"
    )
    return f"{len(folded)} distinct off-CPU stacks captured (top {TOP_STACKS} in offcpu-summary.txt)"


# ---------------------------------------------------------------------------
# Symbolized all-thread backtraces (gdb, falling back to eu-stack).
# ---------------------------------------------------------------------------


def detect_backtrace_tool() -> str | None:
    if shutil.which("gdb"):
        return "gdb"
    if shutil.which("eu-stack"):
        return "eu-stack"
    return None


def capture_backtrace(
    tool: str, pid: int, *, out_dir: Path | None = None, stderr_sink=None
) -> str | None:
    if tool == "gdb":
        argv = maybe_sudo(
            [
                "gdb",
                "-batch",
                "-p",
                str(pid),
                "-ex",
                "set pagination off",
                "-ex",
                "thread apply all bt",
            ]
        )
    else:
        argv = maybe_sudo(["eu-stack", "-p", str(pid)])
    return _run_best_effort(
        argv,
        timeout=BACKTRACE_TIMEOUT_SECS,
        label=f"backtrace-{tool}-{pid}",
        out_dir=out_dir,
        stderr_sink=stderr_sink,
    )


def capture_all_backtraces(
    pids: list[int], out_dir: Path, *, stderr_sink=None
) -> dict[int, str]:
    tool = detect_backtrace_tool()
    results: dict[int, str] = {}
    if tool is None:
        return results
    backtrace_dir = out_dir / "backtraces"
    backtrace_dir.mkdir(parents=True, exist_ok=True)
    for pid in pids:
        text = capture_backtrace(tool, pid, out_dir=out_dir, stderr_sink=stderr_sink)
        if text and text.strip():
            (backtrace_dir / f"backtrace-{pid}.txt").write_text(text, encoding="utf-8")
            results[pid] = text
    return results


# ---------------------------------------------------------------------------
# Tying it together.
# ---------------------------------------------------------------------------


def build_summary_markdown(
    *,
    root_pid: int,
    descendants: list[ProcEntry],
    services: list[ProcEntry],
    on_cpu_status: str,
    off_cpu_status: str,
    backtrace_pids: list[int],
    elapsed_secs: float,
) -> str:
    return (
        "# soldr cook inspector capture\n\n"
        f"- root pid: {root_pid}\n"
        f"- cook descendants found: {len(descendants)}\n"
        f"- soldr daemon/broker processes found: {len(services)}\n"
        f"- on-CPU sampling: {on_cpu_status}\n"
        f"- off-CPU sampling: {off_cpu_status}\n"
        f"- all-thread backtraces captured for pids: {backtrace_pids or '(none)'}\n"
        f"- capture wall time: {elapsed_secs:.1f}s\n\n"
        "See process-table.txt, thread-stacks.txt, oncpu-summary.txt, "
        "offcpu-summary.txt and backtraces/*.txt for the raw detail; "
        "REPORT.txt concatenates all of it for a CI log group.\n"
    )


def build_report_text(
    *,
    summary: str,
    backtraces: dict[int, str],
    thread_stacks_text: str,
    oncpu_summary_text: str,
    offcpu_summary_text: str,
) -> str:
    parts = [
        summary,
        f"## top {TOP_STACKS} on-CPU stacks",
        oncpu_summary_text,
        f"## top {TOP_STACKS} off-CPU (blocking) stacks",
        offcpu_summary_text,
        "## per-thread kernel stacks + wchan (off-CPU, from /proc)",
        thread_stacks_text,
        "## all-thread backtraces (gdb/eu-stack)",
    ]
    if not backtraces:
        parts.append("(none captured -- gdb/eu-stack unavailable or ptrace denied)")
    else:
        for pid in sorted(backtraces):
            parts.append(f"--- pid {pid} ---")
            parts.append(backtraces[pid])
    return "\n".join(parts) + "\n"


def run_inspection(
    root_pid: int,
    out_dir: Path,
    sample_secs: float = DEFAULT_SAMPLE_SECS,
    proc_root: Path = Path("/proc"),
    *,
    stderr_sink=None,
) -> Path:
    """Best-effort forensic capture of `root_pid`'s process tree plus any
    discoverable soldr daemon/broker processes. Never raises. Returns
    `out_dir` (created if missing), containing `SUMMARY.md` and `REPORT.txt`
    plus the raw per-phase files documented in the module docstring.

    `stderr_sink` (default `sys.stderr`) is where every child process's
    stderr is forwarded live, prefixed by tool label -- see the module
    docstring's "never swallow" rule.
    """
    sink = stderr_sink if stderr_sink is not None else sys.stderr
    out_dir = Path(out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    started = time.monotonic()
    deadline = started + INSPECTOR_BUDGET_SECS

    relax_perf_restrictions_best_effort(out_dir=out_dir, stderr_sink=sink)

    table = build_process_table(proc_root)
    descendants = descendants_of(table, root_pid)
    services = soldr_service_entries(table)
    uptime_ticks = _read_uptime_ticks(proc_root)

    process_table_text = (
        f"== cook process tree (root pid {root_pid}) ==\n"
        f"{format_process_table(descendants, uptime_ticks=uptime_ticks)}\n"
        "== soldr daemon/broker processes ==\n"
        f"{format_process_table(services, uptime_ticks=uptime_ticks)}"
    )
    (out_dir / "process-table.txt").write_text(process_table_text, encoding="utf-8")

    all_pids = [entry.pid for entry in descendants] + [entry.pid for entry in services]
    if root_pid not in all_pids:
        all_pids = [root_pid, *all_pids]

    thread_stacks_text = capture_thread_stacks(
        proc_root, all_pids, use_sudo=True, out_dir=out_dir, stderr_sink=sink
    )
    (out_dir / "thread-stacks.txt").write_text(thread_stacks_text, encoding="utf-8")

    on_cpu_status = "skipped (inspector time budget exhausted before sampling)"
    off_cpu_status = "skipped (inspector time budget exhausted before sampling)"
    if time.monotonic() < deadline:
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            on_future = pool.submit(
                capture_on_cpu, all_pids, out_dir, sample_secs, stderr_sink=sink
            )
            off_future = pool.submit(
                capture_off_cpu, all_pids, out_dir, sample_secs, stderr_sink=sink
            )
            on_cpu_status = on_future.result()
            off_cpu_status = off_future.result()

    backtraces: dict[int, str] = {}
    if time.monotonic() < deadline:
        backtraces = capture_all_backtraces(all_pids, out_dir, stderr_sink=sink)

    oncpu_summary_text = _read_text(out_dir / "oncpu-summary.txt") or "(not captured)\n"
    offcpu_summary_text = (
        _read_text(out_dir / "offcpu-summary.txt") or "(not captured)\n"
    )

    summary = build_summary_markdown(
        root_pid=root_pid,
        descendants=descendants,
        services=services,
        on_cpu_status=on_cpu_status,
        off_cpu_status=off_cpu_status,
        backtrace_pids=sorted(backtraces),
        elapsed_secs=time.monotonic() - started,
    )
    (out_dir / "SUMMARY.md").write_text(summary, encoding="utf-8")

    report = build_report_text(
        summary=summary,
        backtraces=backtraces,
        thread_stacks_text=thread_stacks_text,
        oncpu_summary_text=oncpu_summary_text,
        offcpu_summary_text=offcpu_summary_text,
    )
    (out_dir / "REPORT.txt").write_text(report, encoding="utf-8")

    return out_dir


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root-pid", type=int, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--sample-secs", type=float, default=DEFAULT_SAMPLE_SECS)
    args = parser.parse_args(argv)

    out_dir = run_inspection(args.root_pid, args.out, sample_secs=args.sample_secs)
    print(f"cook_inspect: report written to {out_dir}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
