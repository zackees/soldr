from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = REPO_ROOT / ".github" / "scripts" / "cache_benchmark_report.py"


def test_cache_benchmark_report_writes_json_and_summary(tmp_path: Path) -> None:
    json_path = tmp_path / "cache-benchmark-summary.json"
    summary_path = tmp_path / "step-summary.md"
    www_dir = tmp_path / "site"
    env = os.environ.copy()
    env.update(
        {
            "SCENARIO": "all",
            "THRESHOLD_RATIO": "10",
            "BENCHMARK_COMMAND_TARGET": "x86_64-unknown-linux-gnu",
            "BENCHMARK_SUMMARY_JSON": str(json_path),
            "BENCHMARK_SUMMARY_WWW_DIR": str(www_dir),
            "GITHUB_STEP_SUMMARY": str(summary_path),
            "SWATINEM_CLI_RESULT": "success",
            "SWATINEM_CLI_COLD": "78.70",
            "SWATINEM_CLI_WARM": "6.11",
            "SWATINEM_CLI_SAVED": "72.59",
            "SWATINEM_CLI_RATIO": "12.88",
            "SWATINEM_CLI_HIT": "true",
            "SWATINEM_CLI_HIT_DETAIL": "backend=swatinem;exact_hit=true",
            "SWATINEM_CORE_RESULT": "success",
            "SWATINEM_CORE_COLD": "74.88",
            "SWATINEM_CORE_WARM": "6.10",
            "SWATINEM_CORE_SAVED": "68.78",
            "SWATINEM_CORE_RATIO": "12.28",
            "SWATINEM_CORE_HIT": "true",
            "SWATINEM_CORE_HIT_DETAIL": "backend=swatinem;exact_hit=true",
            "ZCCACHE_CLI_RESULT": "success",
            "ZCCACHE_CLI_COLD": "74.55",
            "ZCCACHE_CLI_WARM": "1.72",
            "ZCCACHE_CLI_SAVED": "72.83",
            "ZCCACHE_CLI_RATIO": "43.34",
            "ZCCACHE_CLI_HIT": "true",
            "ZCCACHE_CLI_HIT_DETAIL": (
                "backend=zccache;compilation=true;target=true;registry=true"
            ),
            "ZCCACHE_CORE_RESULT": "success",
            "ZCCACHE_CORE_COLD": "75.38",
            "ZCCACHE_CORE_WARM": "1.46",
            "ZCCACHE_CORE_SAVED": "73.92",
            "ZCCACHE_CORE_RATIO": "51.63",
            "ZCCACHE_CORE_HIT": "true",
            "ZCCACHE_CORE_HIT_DETAIL": (
                "backend=zccache;compilation=true;target=true;registry=true"
            ),
        }
    )

    subprocess.run([sys.executable, str(SCRIPT_PATH)], check=True, env=env, cwd=REPO_ROOT)

    report = json.loads(json_path.read_text(encoding="utf-8"))
    assert report["workflow"] == "cache-benchmark.yml"
    assert report["threshold_ratio"] == 10.0
    assert (
        report["benchmarked_command"]
        == "soldr cargo build --package soldr-cli --release --locked --target x86_64-unknown-linux-gnu"
    )
    assert any(
        item["command"] == "soldr cargo fmt --all -- --check"
        for item in report["command_reference"]
    )
    assert any(item["command"] == "soldr status --json" for item in report["command_reference"])
    assert any(
        item["command"]
        == "soldr cargo clippy --workspace --all-targets --locked -- -D warnings"
        for item in report["command_reference"]
    )

    cli_mutation = next(
        mutation for mutation in report["mutations"] if mutation["mutation"] == "soldr-cli"
    )
    assert cli_mutation["leader_backend"] == "zccache"
    assert cli_mutation["leader_percent_less_wall_time_than_bare"] == 97.69
    assert cli_mutation["leader_percent_less_wall_time_than_runner_up"] == 71.85

    summary = summary_path.read_text(encoding="utf-8")
    assert "cache-benchmark-summary.json" in summary
    assert "`soldr-cli`: `zccache` is best, `97.69%` less wall time than bare" in summary

    www_json = json.loads((www_dir / "latest.json").read_text(encoding="utf-8"))
    assert www_json["workflow"] == "cache-benchmark.yml"
    www_html = (www_dir / "index.html").read_text(encoding="utf-8")
    assert "<title>soldr rendered benchmarks</title>" in www_html
    assert "<th>Command</th>" in www_html
    assert "soldr cargo build --package soldr-cli --release --locked --target x86_64-unknown-linux-gnu" in www_html
    assert "soldr status --json" in www_html
    assert "soldr cargo fmt --all -- --check" in www_html
    assert "soldr cargo clippy --workspace --all-targets --locked -- -D warnings" in www_html
    assert (www_dir / ".nojekyll").exists()
