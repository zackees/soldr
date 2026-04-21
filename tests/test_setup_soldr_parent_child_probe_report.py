from __future__ import annotations

import importlib.util
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = REPO_ROOT / ".github" / "scripts" / "setup_soldr_parent_child_probe_report.py"


def _load_module():
    spec = importlib.util.spec_from_file_location(
        "setup_soldr_parent_child_probe_report", SCRIPT_PATH
    )
    module = importlib.util.module_from_spec(spec)
    assert spec is not None
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


def test_build_report_includes_target_cache_and_zccache_status() -> None:
    module = _load_module()

    report = module.build_report(
        {
            "run_url": "https://github.com/zackees/soldr/actions/runs/1",
            "ref_name": "cache-probe/zccache-parent-child",
            "sha": "abc123",
            "cold_result": "success",
            "warm_result": "success",
            "cold_seconds": "37.00",
            "warm_seconds": "0.50",
            "setup_cache_hit": "true",
            "build_cache_hit": "true",
            "target_cache_hit": "true",
            "soldr_version": "0.7.5",
            "toolchain": "1.94.1",
            "zccache_artifact_cache_dir": "/tmp/setup-soldr/soldr/cache/zccache",
            "zccache_status_lines": [
                "Compilations: 227 total (181 cached, 0 cold, 44 non-cacheable)",
                "Hit rate: 100.0%",
            ],
        }
    )

    assert report.startswith("### Parent-to-child zccache cache report\n")
    assert "- target-cache hit: `true`" in report
    assert "- warm soldr build seconds: `0.50`" in report
    assert "- speedup vs cold control: `74.00x`" in report
    assert "- sub-second target met: `yes`" in report
    assert "- dominant remaining cost: `none observed; sub-second target met`" in report
    assert "Compilations: 227 total" in report


def test_build_report_identifies_missing_target_cache_as_remaining_cost() -> None:
    module = _load_module()

    report = module.build_report(
        {
            "cold_seconds": "37.00",
            "warm_seconds": "35.00",
            "setup_cache_hit": "true",
            "build_cache_hit": "true",
            "target_cache_hit": "false",
            "zccache_status_lines": [],
        }
    )

    assert "- sub-second target met: `no`" in report
    assert (
        "- dominant remaining cost: `target cache was not an exact hit; inspect restore-key fallback logs`"
        in report
    )
