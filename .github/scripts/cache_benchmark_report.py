#!/usr/bin/env python3
"""Generate the top-level cache benchmark report and JSON artifact."""

from __future__ import annotations

import json
import math
import os
from collections import defaultdict
from pathlib import Path
from typing import Any


SCENARIOS = [
    {
        "backend": "swatinem",
        "mutation": "soldr-cli",
        "label": "top-crate edit (`crates/soldr-cli/src/main.rs`)",
        "env_prefix": "SWATINEM_CLI",
    },
    {
        "backend": "swatinem",
        "mutation": "soldr-core",
        "label": "lower-crate edit (`crates/soldr-core/src/lib.rs`)",
        "env_prefix": "SWATINEM_CORE",
    },
    {
        "backend": "zccache",
        "mutation": "soldr-cli",
        "label": "top-crate edit (`crates/soldr-cli/src/main.rs`)",
        "env_prefix": "ZCCACHE_CLI",
    },
    {
        "backend": "zccache",
        "mutation": "soldr-core",
        "label": "lower-crate edit (`crates/soldr-core/src/lib.rs`)",
        "env_prefix": "ZCCACHE_CORE",
    },
]


def _read_float(value: str) -> float | None:
    if not value:
        return None
    return float(value)


def _read_bool(value: str) -> bool | None:
    if not value:
        return None
    if value == "true":
        return True
    if value == "false":
        return False
    raise ValueError(f"unsupported boolean value: {value!r}")


def _percent_less_time(baseline: float, candidate: float) -> float:
    if baseline <= 0:
        return 0.0
    return ((baseline - candidate) / baseline) * 100.0


def _round_metric(value: float | None) -> float | None:
    if value is None or not math.isfinite(value):
        return None
    return round(value, 2)


def _load_results() -> list[dict[str, Any]]:
    results: list[dict[str, Any]] = []
    for scenario in SCENARIOS:
        prefix = scenario["env_prefix"]
        result = os.environ[f"{prefix}_RESULT"]
        cold_seconds = _read_float(os.environ.get(f"{prefix}_COLD", ""))
        warm_seconds = _read_float(os.environ.get(f"{prefix}_WARM", ""))
        saved_seconds = _read_float(os.environ.get(f"{prefix}_SAVED", ""))
        speedup_ratio = _read_float(os.environ.get(f"{prefix}_RATIO", ""))
        cache_hit = _read_bool(os.environ.get(f"{prefix}_HIT", ""))
        cache_hit_detail = os.environ.get(f"{prefix}_HIT_DETAIL", "")

        percent_less_wall_time_than_bare = None
        if cold_seconds is not None and warm_seconds is not None:
            percent_less_wall_time_than_bare = _percent_less_time(cold_seconds, warm_seconds)

        results.append(
            {
                "backend": scenario["backend"],
                "mutation": scenario["mutation"],
                "label": scenario["label"],
                "result": result,
                "cold_seconds": _round_metric(cold_seconds),
                "warm_seconds": _round_metric(warm_seconds),
                "saved_seconds": _round_metric(saved_seconds),
                "speedup_ratio": _round_metric(speedup_ratio),
                "cache_hit": cache_hit,
                "cache_hit_detail": cache_hit_detail or None,
                "percent_less_wall_time_than_bare": _round_metric(
                    percent_less_wall_time_than_bare
                ),
            }
        )
    return results


def _build_report(results: list[dict[str, Any]]) -> dict[str, Any]:
    by_mutation: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for result in results:
        by_mutation[result["mutation"]].append(result)

    mutation_summaries: list[dict[str, Any]] = []
    for mutation, mutation_results in by_mutation.items():
        successful = [
            result
            for result in mutation_results
            if result["result"] == "success" and result["warm_seconds"] is not None
        ]
        successful.sort(key=lambda result: result["warm_seconds"])

        leader = successful[0] if successful else None
        runner_up = successful[1] if len(successful) > 1 else None
        leader_advantage = None
        if leader and runner_up:
            leader_advantage = _percent_less_time(
                runner_up["warm_seconds"], leader["warm_seconds"]
            )

        mutation_summaries.append(
            {
                "mutation": mutation,
                "label": mutation_results[0]["label"],
                "leader_backend": leader["backend"] if leader else None,
                "leader_percent_less_wall_time_than_bare": (
                    leader["percent_less_wall_time_than_bare"] if leader else None
                ),
                "runner_up_backend": runner_up["backend"] if runner_up else None,
                "leader_percent_less_wall_time_than_runner_up": _round_metric(
                    leader_advantage
                ),
                "results": mutation_results,
            }
        )

    mutation_summaries.sort(key=lambda item: item["mutation"])

    return {
        "workflow": "cache-benchmark.yml",
        "requested_scenario": os.environ["SCENARIO"],
        "threshold_ratio": _round_metric(float(os.environ["THRESHOLD_RATIO"])),
        "metric_definition": {
            "percent_less_wall_time_than_bare": (
                "(cold_seconds - warm_seconds) / cold_seconds * 100"
            ),
            "leader_percent_less_wall_time_than_runner_up": (
                "(runner_up_warm_seconds - leader_warm_seconds) / "
                "runner_up_warm_seconds * 100"
            ),
        },
        "mutations": mutation_summaries,
    }


def _write_json_report(report: dict[str, Any]) -> None:
    output_path = Path(os.environ["BENCHMARK_SUMMARY_JSON"])
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")


def _build_summary_lines(report: dict[str, Any]) -> list[str]:
    lines = [
        "### Cache Benchmark Summary",
        "",
        f"- requested scenario: `{report['requested_scenario']}`",
        f"- threshold ratio: `{report['threshold_ratio']:.2f}x`",
        "- primary metric: percent less wall time than bare build",
        "- secondary metric: leader advantage over the next-best cache backend",
        "- artifact: `cache-benchmark-summary.json`",
        "",
        "### Leaders",
        "",
    ]

    for mutation in report["mutations"]:
        leader_backend = mutation["leader_backend"]
        if leader_backend is None:
            lines.append(f"- `{mutation['mutation']}`: no successful benchmark results")
            continue

        leader_vs_bare = mutation["leader_percent_less_wall_time_than_bare"]
        runner_up_backend = mutation["runner_up_backend"]
        leader_vs_runner_up = mutation["leader_percent_less_wall_time_than_runner_up"]
        line = (
            f"- `{mutation['mutation']}`: `{leader_backend}` is best, "
            f"`{leader_vs_bare:.2f}%` less wall time than bare"
        )
        if runner_up_backend and leader_vs_runner_up is not None:
            line += (
                f", `{leader_vs_runner_up:.2f}%` less wall time than `{runner_up_backend}`"
            )
        lines.append(line)

    lines.extend(
        [
            "",
            "### Percent Less Wall Time Than Bare",
            "",
            "| mutation | backend | result | % less wall time than bare |",
            "| --- | --- | --- | ---: |",
        ]
    )

    for mutation in report["mutations"]:
        for result in mutation["results"]:
            percent = result["percent_less_wall_time_than_bare"]
            percent_display = f"{percent:.2f}%" if percent is not None else "n/a"
            lines.append(
                f"| `{result['mutation']}` | `{result['backend']}` | "
                f"`{result['result']}` | `{percent_display}` |"
            )

    return lines


def _append_step_summary(report: dict[str, Any]) -> None:
    summary_path = os.environ.get("GITHUB_STEP_SUMMARY")
    if not summary_path:
        return
    summary_lines = _build_summary_lines(report)
    with Path(summary_path).open("a", encoding="utf-8") as handle:
        handle.write("\n".join(summary_lines) + "\n")


def main() -> None:
    report = _build_report(_load_results())
    _write_json_report(report)
    _append_step_summary(report)


if __name__ == "__main__":
    main()
