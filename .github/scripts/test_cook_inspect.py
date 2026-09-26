"""Tests for cook_inspect.py (soldr#3043 cook-timeout inspector follow-up).

No real `perf`/`gdb`/`sudo` invocation here: process discovery is exercised
against a fake `/proc` fixture built under `tmp_path`, and every native-tool
call is either monkeypatched away (`shutil.which` returns `None`) or replaced
with a fake `_run_best_effort` where the test only cares about wiring, not
the real tool's output.
"""

from __future__ import annotations

import io
import types
from pathlib import Path

import pytest
from _script_loader import load_script_module

SCRIPT = Path(__file__).resolve().parent / "cook_inspect.py"


@pytest.fixture(scope="module")
def mod():
    return load_script_module(SCRIPT, "cook_inspect")


# ---------------------------------------------------------------------------
# Fake /proc fixture helper.
# ---------------------------------------------------------------------------


def _write_proc_entry(
    proc_root: Path,
    pid: int,
    *,
    ppid: int,
    comm: str,
    cmdline_parts: list[str],
    rss_kb: int = 1024,
    utime: int = 0,
    stime: int = 0,
    starttime: int = 0,
    wchan: str = "0",
    tasks: dict[int, tuple[str, str]] | None = None,
) -> None:
    """Write a fake `/proc/<pid>/{stat,cmdline,status,wchan,task/*}` tree.

    `rest` mirrors real `/proc/<pid>/stat` fields 4.. (state is field 3,
    i.e. `rest[0]`) with `utime`/`stime` at indices 11/12 and `starttime` at
    19 -- exactly what `cook_inspect._parse_stat` reads.
    """
    pid_dir = proc_root / str(pid)
    pid_dir.mkdir(parents=True, exist_ok=True)
    rest = ["S", str(ppid), "1", "1", "0", "-1", "0", "0", "0", "0", "0"]
    rest += [str(utime), str(stime), "0", "0", "20", "0", "4", "0", str(starttime)]
    (pid_dir / "stat").write_text(
        f"{pid} ({comm}) {' '.join(rest)}\n", encoding="utf-8"
    )
    (pid_dir / "cmdline").write_text(
        "\x00".join(cmdline_parts) + "\x00", encoding="utf-8"
    )
    (pid_dir / "status").write_text(
        f"Name:\t{comm}\nVmRSS:\t {rss_kb} kB\n", encoding="utf-8"
    )
    (pid_dir / "wchan").write_text(wchan, encoding="utf-8")

    task_dir = pid_dir / "task"
    task_dir.mkdir(exist_ok=True)
    tasks = tasks if tasks is not None else {pid: ("(unreadable stack)", wchan)}
    for tid, (stack_text, thread_wchan) in tasks.items():
        tdir = task_dir / str(tid)
        tdir.mkdir(exist_ok=True)
        (tdir / "stack").write_text(stack_text, encoding="utf-8")
        (tdir / "wchan").write_text(thread_wchan, encoding="utf-8")


# ---------------------------------------------------------------------------
# fold_perf_script (pure).
# ---------------------------------------------------------------------------

PERF_SCRIPT_FIXTURE = """\
cook 1234 5678.001: 1 cycles:
\t    ffffff  frame_a+0x10 (/lib/a.so)
\t    ffffff  frame_b+0x20 (/lib/b.so)
\t    ffffff  frame_c+0x30 (/lib/c.so)

cook 1234 5678.002: 1 cycles:
\t    ffffff  frame_a+0x10 (/lib/a.so)
\t    ffffff  frame_b+0x20 (/lib/b.so)
\t    ffffff  frame_c+0x30 (/lib/c.so)

rustc 4321 5678.003: 1 cycles:
\t    ffffff  frame_x+0x1 (/lib/x.so)
"""


def test_fold_perf_script_merges_identical_logical_stacks(mod):
    folded = mod.fold_perf_script(PERF_SCRIPT_FIXTURE)
    stacks = dict(folded)
    assert sum(stacks.values()) == 3
    # The two identical `cook` stanzas merge into one stack with count 2; the
    # single distinct `rustc` stanza stays at count 1.
    assert sorted(stacks.values()) == [1, 2]
    assert any(
        stack.startswith("frame_c+0x30")
        for stack, count in stacks.items()
        if count == 2
    )


def test_fold_perf_script_orders_by_count_descending(mod):
    text = (
        "cook 1 1.0: cycles:\n\tX common_frame\n\n"
        "cook 1 1.1: cycles:\n\tX common_frame\n\n"
        "cook 1 1.2: cycles:\n\tX rare_frame\n\n"
    )
    folded = mod.fold_perf_script(text)
    assert folded[0] == ("common_frame", 2)
    assert folded[1] == ("rare_frame", 1)


def test_fold_perf_script_respects_top_n(mod):
    text = "".join(f"cook 1 {i}.0: cycles:\n\tX frame_{i}\n\n" for i in range(10))
    folded = mod.fold_perf_script(text, top_n=3)
    assert len(folded) == 3


def test_fold_perf_script_empty_text_is_empty(mod):
    assert mod.fold_perf_script("") == []


def test_render_folded_stacks_reports_no_samples(mod):
    assert "no samples" in mod.render_folded_stacks([])


def test_render_folded_stacks_includes_counts(mod):
    text = mod.render_folded_stacks([("a;b;c", 7)])
    assert "7" in text
    assert "a;b;c" in text


# ---------------------------------------------------------------------------
# Process-tree discovery from a fake /proc fixture.
# ---------------------------------------------------------------------------


def test_build_process_table_reads_every_pid(mod, tmp_path):
    proc_root = tmp_path / "proc"
    _write_proc_entry(
        proc_root, 100, ppid=1, comm="soldr", cmdline_parts=["soldr", "cook"]
    )
    _write_proc_entry(
        proc_root, 101, ppid=100, comm="cargo", cmdline_parts=["cargo", "build"]
    )
    table = mod.build_process_table(proc_root)
    assert set(table) == {100, 101}
    assert table[101].ppid == 100
    assert table[100].cmdline == "soldr cook"


def test_build_process_table_skips_non_numeric_and_vanished_entries(mod, tmp_path):
    proc_root = tmp_path / "proc"
    proc_root.mkdir()
    (proc_root / "self").mkdir()  # non-numeric, must be skipped
    (proc_root / "999").mkdir()  # numeric dir with no stat file (exited pid)
    table = mod.build_process_table(proc_root)
    assert table == {}


def test_descendants_of_walks_the_whole_subtree(mod, tmp_path):
    proc_root = tmp_path / "proc"
    _write_proc_entry(proc_root, 1, ppid=0, comm="init", cmdline_parts=["init"])
    _write_proc_entry(
        proc_root, 100, ppid=1, comm="soldr", cmdline_parts=["soldr", "cook"]
    )
    _write_proc_entry(proc_root, 101, ppid=100, comm="cargo", cmdline_parts=["cargo"])
    _write_proc_entry(proc_root, 102, ppid=101, comm="rustc", cmdline_parts=["rustc"])
    _write_proc_entry(
        proc_root, 200, ppid=1, comm="unrelated", cmdline_parts=["unrelated"]
    )
    table = mod.build_process_table(proc_root)
    descendants = mod.descendants_of(table, 100)
    assert {entry.pid for entry in descendants} == {101, 102}


def test_soldr_service_entries_finds_daemon_and_broker_by_cmdline(mod, tmp_path):
    proc_root = tmp_path / "proc"
    _write_proc_entry(
        proc_root, 5000, ppid=1, comm="soldr", cmdline_parts=["soldr", "daemon", "run"]
    )
    _write_proc_entry(
        proc_root, 5001, ppid=1, comm="soldr", cmdline_parts=["soldr", "broker", "run"]
    )
    _write_proc_entry(
        proc_root, 5002, ppid=1, comm="soldr", cmdline_parts=["soldr", "cook"]
    )
    _write_proc_entry(
        proc_root, 5003, ppid=1, comm="cargo", cmdline_parts=["cargo", "build"]
    )
    table = mod.build_process_table(proc_root)
    services = mod.soldr_service_entries(table)
    assert {entry.pid for entry in services} == {5000, 5001}


def test_soldr_service_entries_are_not_required_to_be_descendants(mod, tmp_path):
    """The whole point: daemon/broker sit outside the cook process tree."""
    proc_root = tmp_path / "proc"
    _write_proc_entry(
        proc_root, 100, ppid=1, comm="soldr", cmdline_parts=["soldr", "cook"]
    )
    _write_proc_entry(
        proc_root, 9000, ppid=1, comm="soldr", cmdline_parts=["soldr", "daemon"]
    )
    table = mod.build_process_table(proc_root)
    descendants = mod.descendants_of(table, 100)
    services = mod.soldr_service_entries(table)
    assert 9000 not in {entry.pid for entry in descendants}
    assert 9000 in {entry.pid for entry in services}


def test_format_process_table_reports_none_found_for_empty_list(mod):
    assert mod.format_process_table([]) == "(none found)\n"


def test_format_process_table_includes_cpu_time_and_cmd(mod, tmp_path):
    proc_root = tmp_path / "proc"
    _write_proc_entry(
        proc_root,
        100,
        ppid=1,
        comm="soldr",
        cmdline_parts=["soldr", "cook"],
        utime=500,
        stime=100,
        rss_kb=2048,
    )
    entry = mod.build_process_table(proc_root)[100]
    text = mod.format_process_table([entry])
    assert "pid=100" in text
    assert "cmd=soldr cook" in text
    assert "rss_kb=2048" in text
    # (500 + 100) / CLK_TCK(100) == 6.0s
    assert "cpu_time=6.0s" in text


# ---------------------------------------------------------------------------
# Thread stacks: direct read succeeds, falls back to sudo when refused.
# ---------------------------------------------------------------------------


def test_read_thread_stack_reads_directly_when_permitted(mod, tmp_path):
    proc_root = tmp_path / "proc"
    _write_proc_entry(
        proc_root,
        100,
        ppid=1,
        comm="soldr",
        cmdline_parts=["soldr"],
        tasks={100: ("frame_a\nframe_b\n", "futex_wait")},
    )
    stack, wchan = mod.read_thread_stack(proc_root, 100, 100, use_sudo=True)
    assert "frame_a" in stack
    assert wchan == "futex_wait"


def test_read_thread_stack_falls_back_to_sudo_when_direct_read_fails(
    mod, tmp_path, monkeypatch
):
    proc_root = tmp_path / "proc"  # no such pid on disk -> direct read fails
    calls = []

    def fake_run_best_effort(argv, **kwargs):
        calls.append(argv)
        if argv[-1].endswith("/stack"):
            return "sudo-read-stack\n"
        return "sudo-read-wchan"

    monkeypatch.setattr(mod, "_run_best_effort", fake_run_best_effort)
    stack, wchan = mod.read_thread_stack(proc_root, 999, 999, use_sudo=True)
    assert stack == "sudo-read-stack\n"
    assert wchan == "sudo-read-wchan"
    assert len(calls) == 2
    assert all(argv[:2] == ["sudo", "-n"] for argv in calls)


def test_read_thread_stack_without_sudo_reports_unreadable(mod, tmp_path):
    proc_root = tmp_path / "proc"
    stack, wchan = mod.read_thread_stack(proc_root, 999, 999, use_sudo=False)
    assert stack == "(unreadable)"
    assert wchan == "(unreadable)"


# ---------------------------------------------------------------------------
# maybe_sudo.
# ---------------------------------------------------------------------------


def test_maybe_sudo_prefixes_when_not_root(mod, monkeypatch):
    monkeypatch.setattr(mod.os, "geteuid", lambda: 1000, raising=False)
    assert mod.maybe_sudo(["perf", "record"]) == ["sudo", "-n", "perf", "record"]


def test_maybe_sudo_is_a_no_op_as_root(mod, monkeypatch):
    monkeypatch.setattr(mod.os, "geteuid", lambda: 0, raising=False)
    assert mod.maybe_sudo(["perf", "record"]) == ["perf", "record"]


# ---------------------------------------------------------------------------
# Degrading cleanly when native tools are absent (monkeypatch shutil.which).
# ---------------------------------------------------------------------------


def test_capture_on_cpu_reports_missing_perf(mod, tmp_path, monkeypatch):
    monkeypatch.setattr(mod.shutil, "which", lambda name: None)
    status = mod.capture_on_cpu([123], tmp_path, 1.0)
    assert "perf not found" in status


def test_capture_off_cpu_reports_missing_both_tools(mod, tmp_path, monkeypatch):
    monkeypatch.setattr(mod.shutil, "which", lambda name: None)
    status = mod.capture_off_cpu([123], tmp_path, 1.0)
    assert "neither offcputime-bpfcc nor perf" in status


def test_capture_off_cpu_prefers_bpfcc_when_present(mod, tmp_path, monkeypatch):
    monkeypatch.setattr(
        mod.shutil,
        "which",
        lambda name: (
            "/usr/bin/offcputime-bpfcc" if name == "offcputime-bpfcc" else None
        ),
    )
    monkeypatch.setattr(
        mod, "_run_best_effort", lambda argv, **kwargs: "off-cpu output\n"
    )
    status = mod.capture_off_cpu([123], tmp_path, 1.0)
    assert "offcputime-bpfcc" in status
    assert (tmp_path / "offcpu-bpfcc-123.txt").read_text(
        encoding="utf-8"
    ) == "off-cpu output\n"


def test_detect_backtrace_tool_prefers_gdb(mod, monkeypatch):
    monkeypatch.setattr(
        mod.shutil,
        "which",
        lambda name: f"/usr/bin/{name}" if name in ("gdb", "eu-stack") else None,
    )
    assert mod.detect_backtrace_tool() == "gdb"


def test_detect_backtrace_tool_falls_back_to_eu_stack(mod, monkeypatch):
    monkeypatch.setattr(
        mod.shutil,
        "which",
        lambda name: "/usr/bin/eu-stack" if name == "eu-stack" else None,
    )
    assert mod.detect_backtrace_tool() == "eu-stack"


def test_detect_backtrace_tool_is_none_when_neither_present(mod, monkeypatch):
    monkeypatch.setattr(mod.shutil, "which", lambda name: None)
    assert mod.detect_backtrace_tool() is None


def test_capture_all_backtraces_is_empty_dict_with_no_tool(mod, tmp_path, monkeypatch):
    monkeypatch.setattr(mod.shutil, "which", lambda name: None)
    assert mod.capture_all_backtraces([123], tmp_path) == {}


def test_run_inspection_degrades_cleanly_with_no_native_tools(
    mod, tmp_path, monkeypatch
):
    """The full orchestration must never raise, and must still produce
    SUMMARY.md + REPORT.txt, even when perf/gdb/offcputime-bpfcc/sudo are
    all unavailable."""
    monkeypatch.setattr(mod.shutil, "which", lambda name: None)
    monkeypatch.setattr(mod, "_run_best_effort", lambda argv, **kwargs: None)

    proc_root = tmp_path / "proc"
    _write_proc_entry(
        proc_root, 100, ppid=1, comm="soldr", cmdline_parts=["soldr", "cook"]
    )
    out_dir = tmp_path / "capture-1"
    sink = io.StringIO()

    result_dir = mod.run_inspection(
        100, out_dir, sample_secs=1.0, proc_root=proc_root, stderr_sink=sink
    )

    assert result_dir == out_dir
    assert (out_dir / "SUMMARY.md").is_file()
    assert (out_dir / "REPORT.txt").is_file()
    summary = (out_dir / "SUMMARY.md").read_text(encoding="utf-8")
    assert "perf not found" in summary or "not found on PATH" in summary


# ---------------------------------------------------------------------------
# "Never swallow": stderr is forwarded and archived, never DEVNULL'd.
# ---------------------------------------------------------------------------


def test_run_best_effort_forwards_stderr_to_sink(mod, monkeypatch):
    fake_completed = types.SimpleNamespace(
        stdout="the stdout\n", stderr="the stderr\n", returncode=0
    )
    monkeypatch.setattr(mod.subprocess, "run", lambda *a, **k: fake_completed)
    sink = io.StringIO()
    stdout = mod._run_best_effort(["true"], label="mytool", stderr_sink=sink)
    assert stdout == "the stdout\n"
    assert "[mytool] the stderr" in sink.getvalue()


def test_run_best_effort_archives_both_streams_under_out_dir(
    mod, tmp_path, monkeypatch
):
    fake_completed = types.SimpleNamespace(stdout="OUT\n", stderr="ERR\n", returncode=0)
    monkeypatch.setattr(mod.subprocess, "run", lambda *a, **k: fake_completed)
    mod._run_best_effort(
        ["true"], label="mytool", out_dir=tmp_path, stderr_sink=io.StringIO()
    )
    logs = tmp_path / "proc-logs"
    assert "OUT" in (logs / "mytool.stdout.log").read_text(encoding="utf-8")
    assert "ERR" in (logs / "mytool.stderr.log").read_text(encoding="utf-8")


def test_run_best_effort_never_raises_on_missing_binary(mod):
    assert mod._run_best_effort(["/no/such/binary-xyz"], label="missing") is None


def test_run_best_effort_forwards_stderr_even_on_timeout(mod, monkeypatch):
    import subprocess as real_subprocess

    def fake_run(*_args, **_kwargs):
        raise real_subprocess.TimeoutExpired(
            cmd=["slow"], timeout=1.0, output="", stderr="partial err\n"
        )

    monkeypatch.setattr(mod.subprocess, "run", fake_run)
    sink = io.StringIO()
    result = mod._run_best_effort(
        ["slow"], label="slowtool", timeout=1.0, stderr_sink=sink
    )
    assert result is None
    assert "[slowtool] partial err" in sink.getvalue()
    assert "timed out" in sink.getvalue()
