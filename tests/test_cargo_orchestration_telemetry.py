"""Fast coverage for the soldr#2878 N=1/N=2/raised-count evidence runner."""

from __future__ import annotations

import sys
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


def test_snapshot_reads_transient_resource_inputs_and_process_census(
    tmp_path: Path,
) -> None:
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


def test_nested_membership_resolves_below_the_process_visible_cgroup_mount() -> None:
    resolved = telemetry.cgroup_v2_dir_from(
        "0::/runner/step-12\n",
        "36 25 0:32 / /sys/fs/cgroup rw - cgroup2 cgroup rw\n",
    )

    assert resolved == Path("/sys/fs/cgroup/runner/step-12")


def test_mount_subtree_is_removed_before_joining_the_membership_path() -> None:
    resolved = telemetry.cgroup_v2_dir_from(
        "0::/delegated/job/step\n",
        "36 25 0:32 /delegated /sys/fs/cgroup rw - cgroup2 cgroup rw\n",
    )

    assert resolved == Path("/sys/fs/cgroup/job/step")


def test_controlling_resolver_reads_injected_proc_membership_and_mountinfo(
    tmp_path: Path,
) -> None:
    membership = tmp_path / "self.cgroup"
    mountinfo = tmp_path / "self.mountinfo"
    membership.write_text("0::/actions/job-42/step\n", encoding="utf-8")
    mountinfo.write_text(
        "36 25 0:32 / /sys/fs/cgroup rw - cgroup2 cgroup rw\n",
        encoding="utf-8",
    )

    assert telemetry.controlling_cgroup_v2_dir(membership, mountinfo) == Path(
        "/sys/fs/cgroup/actions/job-42/step"
    )


def test_missing_v2_membership_does_not_fall_back_to_a_parent_cgroup() -> None:
    assert (
        telemetry.cgroup_v2_dir_from(
            "0::/runner/step\n",
            "36 25 0:32 / /sys/fs/cgroup rw - tmpfs tmpfs rw\n",
        )
        is None
    )


def test_matrix_requires_an_explicit_opt_in_before_running_raised_count(
    tmp_path: Path,
) -> None:
    with pytest.raises(SystemExit):
        telemetry.parse_args(["--raised-jobs", "8", "--", "soldr", "cargo", "check"])

    parsed = telemetry.parse_args(
        [
            "--raised-jobs",
            "8",
            "--allow-raised-count",
            "--case-root",
            str(tmp_path / "fresh-cases"),
            "--",
            "soldr",
            "cargo",
            "check",
        ]
    )

    assert parsed.jobs == [1, 2, 8]
    assert parsed.case_root == tmp_path / "fresh-cases"
    assert parsed.command == ["soldr", "cargo", "check"]


def test_case_uses_a_fresh_target_and_soldr_cache_tree(tmp_path: Path) -> None:
    cgroup = write_cgroup(
        tmp_path / "cgroup",
        memory_current="1\n",
        memory_peak="1\n",
        memory_swap_current="0\n",
        pids_current="1\n",
        memory_events="oom_kill 0\n",
    )
    case_root = tmp_path / "jobs-2"

    result = telemetry.run_case(
        2,
        [sys.executable, "-c", "print('case output')"],
        case_root=case_root,
        cgroup_root=cgroup,
        interval_seconds=0.001,
    )

    assert result["returncode"] == 0
    assert result["case_root"] == str(case_root)
    assert result["command_log"] == str(case_root / "command.log")
    assert (case_root / "target").is_dir()
    assert (case_root / "soldr-cache").is_dir()
    assert (case_root / "command.log").read_text(encoding="utf-8") == "case output\n"


def test_case_isolates_session_cache_and_stops_its_daemon(tmp_path: Path) -> None:
    cgroup = write_cgroup(
        tmp_path / "cgroup",
        memory_current="1\n",
        memory_peak="1\n",
        memory_swap_current="0\n",
        pids_current="1\n",
        memory_events="oom_kill 0\n",
    )
    case_root = tmp_path / "jobs-1"
    keys = (
        "CARGO_TARGET_DIR",
        "SOLDR_CACHE_DIR",
        "ZCCACHE_CACHE_DIR",
        "SOLDR_CACHE_LIFECYCLE",
        "SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS",
    )
    expression = (
        "import os; print('\\n'.join(os.environ[key] for key in " + repr(keys) + "))"
    )

    telemetry.run_case(
        1,
        [sys.executable, "-c", expression],
        case_root=case_root,
        cgroup_root=cgroup,
        interval_seconds=0.001,
    )

    assert (case_root / "command.log").read_text(encoding="utf-8").splitlines() == [
        str(case_root / "target"),
        str(case_root / "soldr-cache"),
        str(case_root / "soldr-cache" / "cache" / "zccache"),
        "command",
        "30",
    ]


def test_prepared_toolchain_preflight_requires_cargo_proxy_or_rustup(
    tmp_path: Path,
) -> None:
    environment = {"CARGO_HOME": str(tmp_path / "cargo-home"), "PATH": ""}

    assert not telemetry.prepared_cargo_or_rustup_available(environment)

    cargo = tmp_path / "cargo-home" / "bin" / "cargo"
    cargo.parent.mkdir(parents=True)
    cargo.write_text("cargo proxy", encoding="utf-8")

    assert telemetry.prepared_cargo_or_rustup_available(environment)


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
