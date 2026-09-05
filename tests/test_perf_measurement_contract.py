"""Regression tests for causal, repeatable performance measurements."""

from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]


def read(relative: str) -> str:
    return (REPO_ROOT / relative).read_text(encoding="utf-8")


def test_touch_scenario_preserves_target_and_reports_cargo_freshness() -> None:
    script = read("perf/scenarios/touch-no-change/run.sh")

    assert " clean >/dev/null" not in script
    assert "-path '*/target' -prune" in script
    assert "-path '*/.git' -prune" in script
    assert "--message-format=json" in script
    assert "warm_fresh_units" in script
    assert "warm_dirty_units" in script
    assert "warm_rustc_invocations" in script
    assert "cargo_units.py" in script
    assert "--expect-first-party-dirty 1" in script
    assert ".manifest_path == $manifest" in script


def test_perf_builds_are_locked_offline_and_prefetched_before_timing() -> None:
    for relative in (
        "perf/scenarios/touch-no-change/run.sh",
        "perf/scenarios/build-then-check/run.sh",
        "perf/scenarios/cold-tar-untar-warm/run.sh",
        "perf/scenarios/worktree-share/run.sh",
    ):
        script = read(relative)
        assert "measure::prefetch_locked" in script
        assert "--offline" in script


def test_shared_measurement_uses_monotonic_clock_and_aggregate_rss() -> None:
    common = read("perf/lib/common.sh")

    assert "/proc/uptime" in common
    assert "soldr-daemon" in common
    assert "measure::peak_process_tree_rss_bytes" in common
    assert "measure::median_and_mad" in common
    assert "soldr cargo metadata" in common


def test_touch_scenario_retains_three_raw_warm_samples() -> None:
    script = read("perf/scenarios/touch-no-change/run.sh")

    assert "PERF_REPETITIONS:-3" in script
    assert "warm_samples_ms" in script
    assert "warm_mad_ms" in script


def test_matrix_uploads_cargo_unit_logs_and_does_not_mask_scenario_failure() -> None:
    workflow = read(".github/workflows/perf-matrix.yml")

    assert "*/cargo-*.jsonl" in workflow
    assert 'exit "${status}"' in workflow
    assert "scenario ${scenario} failed" in workflow


def test_fixture_extraction_resets_previous_scenario_state() -> None:
    script = read("perf/lib/extract.sh")

    assert 'rm -rf "${DEST}"' in script
    assert script.index('rm -rf "${DEST}"') < script.index('mkdir -p "${DEST}"')


def test_readme_comparison_labels_clean_target_reconstruction() -> None:
    """The README comparison's old 'warm' cell deletes ``target/`` first.

    It measures reconstruction from a warm compiler cache, not Cargo's
    intact-target freshness fast path. Keep its machine-readable data, chart,
    and README wording explicit so readers do not compare it to the latter.
    """
    comparison = read("bench/run_comparison.sh")
    renderer = read("bench/render_comparison_bars.py")
    readme = read("README.md")

    assert 'clean_project "${project}" "${target}"' in comparison
    assert '"Clean-target rebuild (same workspace; warm compiler cache)"' in comparison
    assert (
        '"label": "Clean-target rebuild (same workspace; warm compiler cache)"'
        in renderer
    )
    assert "clean-target reconstruction" in readme
