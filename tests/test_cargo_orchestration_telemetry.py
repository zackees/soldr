"""Fast coverage for the soldr#2878 N=1/N=2/raised-count evidence runner."""

from __future__ import annotations

from pathlib import Path

import pytest

from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
telemetry = load_script_module(
    REPO_ROOT / "ci" / "cargo_orchestration_telemetry.py",
    "cargo_orchestration_telemetry",
)


def write_cgroup(root: Path, **files: str) -> Path:
    root.mkdir(parents=True, exist_ok=True)
    for name, contents in files.items():
        (root / name.replace("_", ".", 1)).write_text(contents, encoding="utf-8")
    return root


def test_snapshot_reads_transient_resource_inputs_and_process_census(tmp_path: Path) -> None:
    cgroup = write_cgroup(
        tmp_path / "cgroup",
        memory_current="1073741824\n",
        memory_peak="3221225472\n",
        memory_swap_current="0\n",
        pids_current="37\n",
        memory_events="oom_kill 1\noom_group_kill 2\n",
    )
    proc = tmp_path / "proc"
    for pid, command in {"11": "cargo", "12": "rustc", "13": "soldr-daemon"}.items():
        directory = proc / pid
        directory.mkdir(parents=True)
        (directory / "comm").write_text(command, encoding="utf-8")

    sample = telemetry.snapshot(cgroup, proc, clock=lambda: 42.5)

    assert sample.monotonic_seconds == 42.5
    assert sample.memory_current_bytes == 1024**3
    assert sample.memory_peak_bytes == 3 * 1024**3
    assert sample.pids_current == 37
    assert sample.memory_events == {"oom_kill": 1, "oom_group_kill": 2}
    assert sample.processes.cargo == 1
    assert sample.processes.compiler == 1
    assert sample.processes.soldr == 1
    assert sample.processes.toolchain == 3


def test_matrix_requires_an_explicit_opt_in_before_running_raised_count() -> None:
    with pytest.raises(SystemExit):
        telemetry.parse_args(["--raised-jobs", "8", "--", "soldr", "cargo", "check"])

    parsed = telemetry.parse_args(
        [
            "--raised-jobs",
            "8",
            "--allow-raised-count",
            "--",
            "soldr",
            "cargo",
            "check",
        ]
    )

    assert parsed.jobs == [1, 2, 8]
    assert parsed.command == ["soldr", "cargo", "check"]


def test_summary_uses_sampled_peak_not_only_post_failure_state() -> None:
    baseline = telemetry.Snapshot(
        monotonic_seconds=0,
        memory_current_bytes=1,
        memory_peak_bytes=9,
        memory_swap_current_bytes=0,
        pids_current=2,
        memory_events={},
        processes=telemetry.ProcessCounts(0, 0, 0, 0),
    )
    transient = telemetry.Snapshot(
        monotonic_seconds=1,
        memory_current_bytes=8,
        memory_peak_bytes=9,
        memory_swap_current_bytes=3,
        pids_current=12,
        memory_events={},
        processes=telemetry.ProcessCounts(2, 4, 1, 7),
    )
    post_failure = telemetry.Snapshot(
        monotonic_seconds=2,
        memory_current_bytes=1,
        memory_peak_bytes=9,
        memory_swap_current_bytes=0,
        pids_current=2,
        memory_events={},
        processes=telemetry.ProcessCounts(0, 0, 0, 0),
    )

    maxima = telemetry.summarize_samples([baseline, transient, post_failure])

    assert maxima["max_memory_current_bytes"] == 8
    assert maxima["max_pids_current"] == 12
    assert maxima["max_cargo_processes"] == 2
    assert maxima["max_compiler_processes"] == 4
    assert maxima["max_toolchain_processes"] == 7


def test_markdown_includes_the_required_cross_run_metrics() -> None:
    rendered = telemetry.format_markdown(
        [
            {
                "requested_jobs": 8,
                "returncode": 1,
                "timed_out": False,
                "wall_time_ms": 1234,
                "memory_events_delta": {"oom_kill": 1},
                "maxima": {
                    "max_memory_current_bytes": 9,
                    "max_pids_current": 12,
                    "max_cargo_processes": 2,
                    "max_compiler_processes": 3,
                    "max_toolchain_processes": 6,
                },
            }
        ]
    )

    assert "max current" in rendered
    assert "max PIDs" in rendered
    assert "max Cargo/compiler/tools" in rendered
    assert "| 8 | 1 | 1234 ms | 9 | 12 | 2/3/6 | 1 |" in rendered
