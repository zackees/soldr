from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]


def test_perf_matrix_fails_zero_hit_miss_stats() -> None:
    workflow = (REPO_ROOT / ".github" / "workflows" / "perf-matrix.yml").read_text()

    assert "hits_key_for()" in workflow
    assert "misses_key_for()" in workflow
    assert "hits + misses <= 0" in workflow
    assert "BAD-STATS" in workflow
    assert "zccache stats were not captured" in workflow


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
