from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]


def test_routine_main_push_does_not_launch_expensive_perf_workflows() -> None:
    matrix = (REPO_ROOT / ".github" / "workflows" / "perf-matrix.yml").read_text()
    stats = (REPO_ROOT / ".github" / "workflows" / "benchmark-stats.yml").read_text()
    matrix_push = matrix.split("  push:\n", 1)[1].split("  schedule:\n", 1)[0]
    assert "      - main\n" not in matrix_push
    assert "  schedule:\n" in matrix
    assert "  push:\n" not in stats.split("  schedule:\n", 1)[0]
    assert "  schedule:\n" in stats


def test_perf_matrix_fails_zero_hit_miss_stats() -> None:
    workflow = (REPO_ROOT / ".github" / "workflows" / "perf-matrix.yml").read_text()

    assert "hits_key_for()" in workflow
    assert "misses_key_for()" in workflow
    assert "hits + misses <= 0" in workflow
    assert "BAD-STATS" in workflow
    assert "zccache stats were not captured" in workflow


def test_perf_matrix_keeps_only_exact_source_binary_cache() -> None:
    workflow = (REPO_ROOT / ".github" / "workflows" / "perf-matrix.yml").read_text()

    restore = workflow.index(
        "uses: actions/cache/restore@", workflow.index("Restore cached soldr binary")
    )
    build = workflow.index("Build soldr (release)")
    save = workflow.index("Save cached soldr binary")
    assert restore < build < save
    assert (
        "uses: actions/cache/save@"
        in workflow[save : workflow.index("Stage binary", save)]
    )
    expected_inputs = "'crates/**', 'Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml', '.cargo/config.toml'"
    assert workflow.count(expected_inputs) == 2
    assert "Swatinem/rust-cache" not in workflow
    assert "perf-registry-soldr-" not in workflow
    assert "v0-rust-perf-build-soldr-" not in workflow
    assert "Restore cargo intermediates" not in workflow


def test_perf_scenarios_read_stats_from_cache_report() -> None:
    for rel in [
        "perf/scenarios/cold-tar-untar-warm/run.sh",
        "perf/scenarios/worktree-share/run.sh",
        "perf/scenarios/touch-no-change/run.sh",
    ]:
        script = (REPO_ROOT / rel).read_text()
        assert "measure::write_cache_report" in script
        assert "measure::cache_report_stat" in script
        assert "measure::session_end_json" not in script
