from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = REPO_ROOT / ".github" / "scripts" / "cache_benchmark_report.py"
CONFIG_PATH = REPO_ROOT / "benchmark.toml"


def _sample_results() -> list[dict[str, object]]:
    return [
        {
            "competitor": "soldr",
            "profile": "release",
            "mutation": "soldr-cli",
            "result": "success",
            "cold_seconds": 76.2,
            "warm_seconds": 1.4,
            "saved_seconds": 74.8,
            "speedup_ratio": 54.43,
            "cache_hit": True,
            "cache_hit_detail": "backend=zccache;same_job_seed=true",
        },
        {
            "competitor": "swatinem",
            "profile": "release",
            "mutation": "soldr-cli",
            "result": "success",
            "cold_seconds": 74.5,
            "warm_seconds": 6.6,
            "saved_seconds": 67.9,
            "speedup_ratio": 11.29,
            "cache_hit": True,
            "cache_hit_detail": "backend=swatinem;same_job_seed=true",
        },
        {
            "competitor": "soldr",
            "profile": "release",
            "mutation": "soldr-core",
            "result": "success",
            "cold_seconds": 76.4,
            "warm_seconds": 1.0,
            "saved_seconds": 75.4,
            "speedup_ratio": 76.4,
            "cache_hit": True,
            "cache_hit_detail": "backend=zccache;same_job_seed=true",
        },
        {
            "competitor": "swatinem",
            "profile": "release",
            "mutation": "soldr-core",
            "result": "success",
            "cold_seconds": 74.5,
            "warm_seconds": 6.4,
            "saved_seconds": 68.1,
            "speedup_ratio": 11.64,
            "cache_hit": True,
            "cache_hit_detail": "backend=swatinem;same_job_seed=true",
        },
        {
            "competitor": "soldr",
            "profile": "quick",
            "mutation": "soldr-cli",
            "result": "success",
            "cold_seconds": 21.8,
            "warm_seconds": 0.9,
            "saved_seconds": 20.9,
            "speedup_ratio": 24.22,
            "cache_hit": True,
            "cache_hit_detail": "backend=zccache;same_job_seed=true",
        },
        {
            "competitor": "swatinem",
            "profile": "quick",
            "mutation": "soldr-cli",
            "result": "success",
            "cold_seconds": 20.9,
            "warm_seconds": 3.4,
            "saved_seconds": 17.5,
            "speedup_ratio": 6.15,
            "cache_hit": True,
            "cache_hit_detail": "backend=swatinem;same_job_seed=true",
        },
        {
            "competitor": "soldr",
            "profile": "quick",
            "mutation": "soldr-core",
            "result": "success",
            "cold_seconds": 22.1,
            "warm_seconds": 0.8,
            "saved_seconds": 21.3,
            "speedup_ratio": 27.63,
            "cache_hit": True,
            "cache_hit_detail": "backend=zccache;same_job_seed=true",
        },
        {
            "competitor": "swatinem",
            "profile": "quick",
            "mutation": "soldr-core",
            "result": "success",
            "cold_seconds": 21.3,
            "warm_seconds": 3.0,
            "saved_seconds": 18.3,
            "speedup_ratio": 7.1,
            "cache_hit": True,
            "cache_hit_detail": "backend=swatinem;same_job_seed=true",
        },
        {
            "competitor": "soldr",
            "profile": "lint",
            "mutation": "soldr-cli",
            "result": "success",
            "cold_seconds": 44.6,
            "warm_seconds": 2.1,
            "saved_seconds": 42.5,
            "speedup_ratio": 21.24,
            "cache_hit": True,
            "cache_hit_detail": "backend=zccache;same_job_seed=true",
        },
        {
            "competitor": "swatinem",
            "profile": "lint",
            "mutation": "soldr-cli",
            "result": "success",
            "cold_seconds": 43.8,
            "warm_seconds": 8.9,
            "saved_seconds": 34.9,
            "speedup_ratio": 4.92,
            "cache_hit": True,
            "cache_hit_detail": "backend=swatinem;same_job_seed=true",
        },
        {
            "competitor": "soldr",
            "profile": "lint",
            "mutation": "soldr-core",
            "result": "success",
            "cold_seconds": 45.0,
            "warm_seconds": 1.9,
            "saved_seconds": 43.1,
            "speedup_ratio": 23.68,
            "cache_hit": True,
            "cache_hit_detail": "backend=zccache;same_job_seed=true",
        },
        {
            "competitor": "swatinem",
            "profile": "lint",
            "mutation": "soldr-core",
            "result": "success",
            "cold_seconds": 44.1,
            "warm_seconds": 8.1,
            "saved_seconds": 36.0,
            "speedup_ratio": 5.44,
            "cache_hit": True,
            "cache_hit_detail": "backend=swatinem;same_job_seed=true",
        },
    ]


def test_cache_benchmark_report_writes_json_and_summary(tmp_path: Path) -> None:
    input_path = tmp_path / "cache-benchmark-results.json"
    json_path = tmp_path / "cache-benchmark-summary.json"
    summary_path = tmp_path / "step-summary.md"
    www_dir = tmp_path / "site"
    input_path.write_text(
        json.dumps({"results": _sample_results()}, indent=2) + "\n",
        encoding="utf-8",
    )

    env = os.environ.copy()
    env.update(
        {
            "SCENARIO": "all",
            "THRESHOLD_RATIO": "10",
            "BENCHMARK_CONFIG_PATH": str(CONFIG_PATH),
            "BENCHMARK_COMMAND_TARGET": "x86_64-unknown-linux-gnu",
            "BENCHMARK_INPUT_JSON": str(input_path),
            "BENCHMARK_SUMMARY_JSON": str(json_path),
            "BENCHMARK_SUMMARY_WWW_DIR": str(www_dir),
            "GITHUB_STEP_SUMMARY": str(summary_path),
        }
    )

    subprocess.run([sys.executable, str(SCRIPT_PATH)], check=True, env=env, cwd=REPO_ROOT)

    report = json.loads(json_path.read_text(encoding="utf-8"))
    assert report["workflow"] == "cache-benchmark.yml"
    assert report["config_path"] == "benchmark.toml"
    assert report["threshold_ratio"] == 10.0
    assert report["site"]["base_competitor"] == "swatinem"
    assert len(report["profiles"]) == 3
    assert len(report["comparisons"]) == 6
    assert len(report["results"]) == 12
    assert any(
        profile["command"]
        == "soldr cargo clippy --workspace --all-targets --locked --target x86_64-unknown-linux-gnu -- -D warnings"
        for profile in report["profiles"]
    )

    quick_cli = next(
        row
        for row in report["comparisons"]
        if row["profile"] == "quick" and row["mutation"] == "soldr-cli"
    )
    assert quick_cli["soldr_vs_base_warm_percent"] == 73.53
    assert quick_cli["competitors"]["soldr"]["backend"] == "zccache"
    assert quick_cli["competitors"]["soldr"]["competitor_label"] == "soldr"
    assert quick_cli["competitors"]["swatinem"]["backend"] == "swatinem"

    summary = summary_path.read_text(encoding="utf-8")
    assert "cache-benchmark-results.json" in summary
    assert "| `Release build` | `Top-crate edit` | `1.40s` | `6.60s` | `78.79%` |" in summary
    assert "| `Lint` | `Lower-crate edit` | `1.90s` | `8.10s` | `76.54%` |" in summary

    www_json = json.loads((www_dir / "latest.json").read_text(encoding="utf-8"))
    assert www_json["headline"].startswith("Across 6 configured comparisons")
    www_html = (www_dir / "index.html").read_text(encoding="utf-8")
    assert "<title>soldr rendered benchmarks</title>" in www_html
    assert "<th>Profile</th>" in www_html
    assert "<th>soldr cold</th>" in www_html
    assert "<th>swatinem warm</th>" in www_html
    assert "<th>Result</th>" not in www_html
    assert "soldr cargo check -p soldr-cli --locked --target x86_64-unknown-linux-gnu" in www_html
    assert "soldr cargo clippy --workspace --all-targets --locked --target x86_64-unknown-linux-gnu -- -D warnings" in www_html
    assert "soldr uses managed zccache internally." in www_html
    assert "latest.json" in www_html
    assert (www_dir / ".nojekyll").exists()
