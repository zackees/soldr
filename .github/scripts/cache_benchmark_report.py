#!/usr/bin/env python3
"""Generate the cache benchmark report and rendered site bundle."""

from __future__ import annotations

import json
import math
import os
import tomllib
from collections import defaultdict
from html import escape
from pathlib import Path
from typing import Any


REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_CONFIG_PATH = REPO_ROOT / "benchmark.toml"
DEFAULT_TARGET = "x86_64-unknown-linux-gnu"


class _SafeFormatDict(dict[str, str]):
    def __missing__(self, key: str) -> str:
        return "{" + key + "}"


def _read_float(value: Any) -> float | None:
    if value in ("", None):
        return None
    return float(value)


def _read_bool(value: Any) -> bool | None:
    if value in ("", None):
        return None
    if isinstance(value, bool):
        return value
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


def _format_seconds(value: float | None) -> str:
    return "n/a" if value is None else f"{value:.2f}s"


def _format_ratio(value: float | None) -> str:
    return "n/a" if value is None else f"{value:.2f}x"


def _format_percent(value: float | None) -> str:
    return "n/a" if value is None else f"{value:.2f}%"


def _format_bool(value: bool | None) -> str:
    if value is None:
        return "n/a"
    return "true" if value else "false"


def _load_config() -> tuple[dict[str, Any], Path]:
    config_path = Path(os.environ.get("BENCHMARK_CONFIG_PATH", DEFAULT_CONFIG_PATH))
    if not config_path.is_absolute():
        config_path = REPO_ROOT / config_path
    config_path = config_path.resolve()
    return tomllib.loads(config_path.read_text(encoding="utf-8")), config_path


def _mutation_by_id(config: dict[str, Any]) -> dict[str, dict[str, Any]]:
    return {mutation["id"]: mutation for mutation in config.get("mutations", [])}


def _format_command(template: str, target: str) -> str:
    return template.format_map(_SafeFormatDict(target=target))


def _load_results(
    config: dict[str, Any],
) -> tuple[list[dict[str, Any]], list[dict[str, Any]], list[dict[str, Any]], list[dict[str, Any]]]:
    input_path = Path(os.environ["BENCHMARK_INPUT_JSON"])
    payload = json.loads(input_path.read_text(encoding="utf-8"))
    raw_results = payload["results"] if isinstance(payload, dict) else payload

    site = config["site"]
    target = os.environ.get("BENCHMARK_COMMAND_TARGET") or site.get(
        "default_target", DEFAULT_TARGET
    )
    competitors = [
        {"id": competitor_id, **competitor}
        for competitor_id, competitor in config["competitors"].items()
        if competitor.get("show", True)
    ]
    competitor_by_id = {competitor["id"]: competitor for competitor in competitors}
    profiles = list(config["profiles"])
    profile_by_id = {profile["id"]: profile for profile in profiles}
    mutations = list(config["mutations"])
    mutation_by_id = _mutation_by_id(config)

    results: list[dict[str, Any]] = []
    for raw_result in raw_results:
        competitor = competitor_by_id[raw_result["competitor"]]
        profile = profile_by_id[raw_result["profile"]]
        mutation = mutation_by_id[raw_result["mutation"]]
        results.append(
            {
                "competitor": competitor["id"],
                "competitor_label": competitor["label"],
                "backend": competitor["backend"],
                "profile": profile["id"],
                "profile_label": profile["label"],
                "mutation": mutation["id"],
                "mutation_label": mutation["label"],
                "mutation_path": mutation["path"],
                "command": _format_command(profile["command"], target),
                "result": raw_result.get("result", "success"),
                "cold_seconds": _round_metric(_read_float(raw_result.get("cold_seconds"))),
                "warm_seconds": _round_metric(_read_float(raw_result.get("warm_seconds"))),
                "saved_seconds": _round_metric(_read_float(raw_result.get("saved_seconds"))),
                "speedup_ratio": _round_metric(_read_float(raw_result.get("speedup_ratio"))),
                "cache_hit": _read_bool(raw_result.get("cache_hit")),
                "cache_hit_detail": raw_result.get("cache_hit_detail") or None,
                "threshold_failed": bool(raw_result.get("threshold_failed", False)),
            }
        )

    return results, competitors, profiles, mutations


def _build_report(
    config: dict[str, Any],
    config_path: Path,
    results: list[dict[str, Any]],
    competitors: list[dict[str, Any]],
    profiles: list[dict[str, Any]],
    mutations: list[dict[str, Any]],
) -> dict[str, Any]:
    site = config["site"]
    base_competitor_id = site["base_competitor"]
    measured_mutation_ids = {result["mutation"] for result in results}
    visible_mutations = [
        mutation for mutation in mutations if mutation["id"] in measured_mutation_ids
    ]
    results_by_key: dict[tuple[str, str], dict[str, dict[str, Any]]] = defaultdict(dict)
    for result in results:
        results_by_key[(result["profile"], result["mutation"])][result["competitor"]] = result

    comparison_rows: list[dict[str, Any]] = []
    for profile in profiles:
        for mutation in visible_mutations:
            key = (profile["id"], mutation["id"])
            competitor_results = results_by_key.get(key, {})
            visible_results = {
                competitor["id"]: competitor_results.get(competitor["id"]) for competitor in competitors
            }

            soldr_result = visible_results.get("soldr")
            base_result = visible_results.get(base_competitor_id)
            soldr_vs_base = None
            if (
                soldr_result
                and base_result
                and soldr_result["result"] == "success"
                and base_result["result"] == "success"
                and soldr_result["warm_seconds"] is not None
                and base_result["warm_seconds"] is not None
            ):
                soldr_vs_base = _round_metric(
                    _percent_less_time(base_result["warm_seconds"], soldr_result["warm_seconds"])
                )

            comparison_rows.append(
                {
                    "profile": profile["id"],
                    "profile_label": profile["label"],
                    "mutation": mutation["id"],
                    "mutation_label": mutation["label"],
                    "competitors": visible_results,
                    "soldr_vs_base_warm_percent": soldr_vs_base,
                }
            )

    comparison_values = [
        row["soldr_vs_base_warm_percent"]
        for row in comparison_rows
        if row["soldr_vs_base_warm_percent"] is not None
    ]
    soldr_wins = sum(1 for value in comparison_values if value > 0)
    headline = "No successful soldr vs swatinem comparisons yet."
    if comparison_values:
        average = sum(comparison_values) / len(comparison_values)
        trend = "faster" if average >= 0 else "slower"
        headline = (
            f"Across {len(comparison_values)} configured comparisons, soldr is "
            f"{abs(average):.2f}% {trend} on warm time than swatinem and leads "
            f"{soldr_wins} rows."
        )

    profile_commands = [
        {
            "id": profile["id"],
            "label": profile["label"],
            "command": _format_command(
                profile["command"],
                os.environ.get("BENCHMARK_COMMAND_TARGET")
                or site.get("default_target", DEFAULT_TARGET),
            ),
        }
        for profile in profiles
    ]

    return {
        "workflow": "cache-benchmark.yml",
        "config_path": str(config_path.relative_to(REPO_ROOT)),
        "requested_scenario": os.environ["SCENARIO"],
        "threshold_ratio": _round_metric(float(os.environ["THRESHOLD_RATIO"])),
        "headline": headline,
        "site": {
            "title": site["title"],
            "soldr_note": site.get("soldr_note"),
            "base_competitor": base_competitor_id,
        },
        "competitors": competitors,
        "profiles": profile_commands,
        "mutations": visible_mutations,
        "comparisons": comparison_rows,
        "results": results,
        "metric_definition": {
            "speedup_ratio": "cold_seconds / warm_seconds",
            "soldr_vs_base_warm_percent": "(base_warm_seconds - soldr_warm_seconds) / base_warm_seconds * 100",
        },
    }


def _write_json_report(report: dict[str, Any]) -> None:
    output_path = Path(os.environ["BENCHMARK_SUMMARY_JSON"])
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")


def _comparison_result(row: dict[str, Any], competitor_id: str) -> dict[str, Any] | None:
    return row["competitors"].get(competitor_id)


def _build_table_rows(report: dict[str, Any]) -> str:
    rows: list[str] = []
    for row in report["comparisons"]:
        soldr = _comparison_result(row, "soldr") or {}
        swatinem = _comparison_result(row, report["site"]["base_competitor"]) or {}
        rows.append(
            "<tr>"
            f"<td>{escape(row['profile_label'])}</td>"
            f"<td>{escape(row['mutation_label'])}</td>"
            f"<td>{_format_seconds(soldr.get('cold_seconds'))}</td>"
            f"<td>{_format_seconds(soldr.get('warm_seconds'))}</td>"
            f"<td>{_format_ratio(soldr.get('speedup_ratio'))}</td>"
            f"<td>{_format_seconds(swatinem.get('cold_seconds'))}</td>"
            f"<td>{_format_seconds(swatinem.get('warm_seconds'))}</td>"
            f"<td>{_format_ratio(swatinem.get('speedup_ratio'))}</td>"
            f"<td>{_format_percent(row['soldr_vs_base_warm_percent'])}</td>"
            "</tr>"
        )
    return "\n".join(rows)


def _build_profile_command_items(report: dict[str, Any]) -> str:
    items: list[str] = []
    for profile in report["profiles"]:
        items.append(
            "<li>"
            f"<strong>{escape(profile['label'])}</strong>: "
            f"<code>{escape(profile['command'])}</code>"
            "</li>"
        )
    return "\n".join(items)


def _build_html_page(report: dict[str, Any]) -> str:
    soldr_note = report["site"].get("soldr_note") or ""
    return f"""<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>{escape(report["site"]["title"])}</title>
    <style>
      body {{
        margin: 0;
        padding: 32px 20px 40px;
        font-family: Arial, sans-serif;
        color: #202426;
        background: #f8f8f6;
      }}
      main {{
        max-width: 1080px;
        margin: 0 auto;
      }}
      h1 {{
        margin: 0 0 12px;
        font-size: 32px;
      }}
      h2 {{
        margin: 28px 0 10px;
        font-size: 22px;
      }}
      p, li {{
        line-height: 1.5;
      }}
      p {{
        margin: 0 0 12px;
      }}
      .meta {{
        color: #4e5a5f;
      }}
      .note {{
        color: #2f3d42;
        background: #eef2f3;
        border: 1px solid #d7dcdf;
        padding: 12px 14px;
      }}
      ul {{
        margin: 0;
        padding-left: 20px;
      }}
      table {{
        width: 100%;
        border-collapse: collapse;
        margin-top: 20px;
        background: #ffffff;
      }}
      th, td {{
        border: 1px solid #d7dcdf;
        padding: 10px 12px;
        text-align: left;
        font-size: 14px;
      }}
      th {{
        background: #eef2f3;
      }}
      tbody tr:nth-child(even) {{
        background: #fafcfc;
      }}
      .footer {{
        margin-top: 18px;
        color: #4e5a5f;
        font-size: 13px;
      }}
      @media (max-width: 900px) {{
        .table-wrap {{
          overflow-x: auto;
        }}
        table {{
          min-width: 880px;
        }}
      }}
    </style>
  </head>
  <body>
    <main>
      <h1>{escape(report["site"]["title"])}</h1>
      <p>{escape(report["headline"])}</p>
      <p class="meta">
        Workflow: {escape(report["workflow"])} |
        Scenario: {escape(report["requested_scenario"])} |
        Threshold: {report["threshold_ratio"]:.2f}x
      </p>
      <p class="note">
        {escape(soldr_note)} Raw detail is published beside this page as
        <a href="latest.json">latest.json</a>.
      </p>
      <div class="table-wrap">
        <table>
          <thead>
            <tr>
              <th>Profile</th>
              <th>Change</th>
              <th>soldr cold</th>
              <th>soldr warm</th>
              <th>soldr speedup</th>
              <th>swatinem cold</th>
              <th>swatinem warm</th>
              <th>swatinem speedup</th>
              <th>soldr vs swatinem</th>
            </tr>
          </thead>
          <tbody>
            {_build_table_rows(report)}
          </tbody>
        </table>
      </div>
      <h2>Benchmarked Commands</h2>
      <ul>
        {_build_profile_command_items(report)}
      </ul>
      <p class="footer">Config: <code>{escape(report["config_path"])}</code>.</p>
    </main>
  </body>
</html>
"""


def _write_www_bundle(report: dict[str, Any]) -> None:
    www_dir = os.environ.get("BENCHMARK_SUMMARY_WWW_DIR")
    if not www_dir:
        return

    output_dir = Path(www_dir)
    output_dir.mkdir(parents=True, exist_ok=True)
    (output_dir / "index.html").write_text(_build_html_page(report), encoding="utf-8")
    (output_dir / "latest.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    (output_dir / ".nojekyll").write_text("", encoding="utf-8")


def _build_summary_lines(report: dict[str, Any]) -> list[str]:
    lines = [
        "### Cache Benchmark Summary",
        "",
        f"- requested scenario: `{report['requested_scenario']}`",
        f"- threshold ratio: `{report['threshold_ratio']:.2f}x`",
        f"- config: `{report['config_path']}`",
        "- artifact: `cache-benchmark-summary.json`",
        "- raw detail artifact: `cache-benchmark-results.json`",
        "",
        "### Warm Comparison",
        "",
        "| profile | change | soldr warm | swatinem warm | soldr vs swatinem |",
        "| --- | --- | ---: | ---: | ---: |",
    ]

    for row in report["comparisons"]:
        soldr = _comparison_result(row, "soldr") or {}
        swatinem = _comparison_result(row, report["site"]["base_competitor"]) or {}
        lines.append(
            f"| `{row['profile_label']}` | `{row['mutation_label']}` | "
            f"`{_format_seconds(soldr.get('warm_seconds'))}` | "
            f"`{_format_seconds(swatinem.get('warm_seconds'))}` | "
            f"`{_format_percent(row['soldr_vs_base_warm_percent'])}` |"
        )

    return lines


def _append_step_summary(report: dict[str, Any]) -> None:
    summary_path = os.environ.get("GITHUB_STEP_SUMMARY")
    if not summary_path:
        return
    summary_lines = _build_summary_lines(report)
    with Path(summary_path).open("a", encoding="utf-8") as handle:
        handle.write("\n".join(summary_lines) + "\n")


def _phase1_result_label(
    mutation_by_id: dict[str, dict[str, Any]], mutation_id: str
) -> str:
    mutation = mutation_by_id.get(mutation_id)
    if mutation is None:
        return f"`{mutation_id}`"
    return f"{mutation['label']} (`{mutation['path']}`)"


def _phase1_issue_target(config: dict[str, Any]) -> str:
    issue_number = config["phase1"].get("issue")
    return f"#{issue_number}" if issue_number is not None else "the Phase 1 tracker issue"


def _build_phase1_issue_comment_lines(
    config: dict[str, Any], payload: dict[str, Any]
) -> list[str]:
    phase1 = config["phase1"]
    mutation_by_id = _mutation_by_id(config)
    runner = payload.get("runner") or phase1["runner"]
    target = payload.get("target") or phase1["target"]
    threshold = float(payload.get("threshold_ratio") or phase1["default_threshold_ratio"])
    cache_backend = payload["cache_backend"]
    issue_comment_lines = [
        "### Phase 1 benchmark results",
        "",
        "- workflow: `cache-benchmark.yml`",
        f"- cache backend under test: `{cache_backend}`",
        f"- threshold used: `{threshold:.2f}x`",
        f"- runner: `{runner}`",
        f"- target: `{target}`",
        "",
    ]

    for result in payload["results"]:
        mutation_id = result["mutation"]
        label = _phase1_result_label(mutation_by_id, mutation_id)
        status = result.get("result", "success")
        cold = _read_float(result.get("cold_seconds"))
        warm = _read_float(result.get("warm_seconds"))
        saved = _read_float(result.get("saved_seconds"))
        ratio = _read_float(result.get("speedup_ratio"))
        cache_hit = _read_bool(result.get("cache_hit"))
        hit_detail = result.get("cache_hit_detail") or "n/a"

        if status == "skipped":
            continue

        issue_summary = [
            f"- {label}: job result `{status}`",
            f"  cache detail: `{hit_detail}`",
        ]
        if status == "success":
            issue_summary[0] = (
                f"- {label}: cold `{_format_seconds(cold)}`, warm `{_format_seconds(warm)}`, "
                f"saved `{_format_seconds(saved)}`, speedup `{_format_ratio(ratio)}`, "
                f"cache hit `{_format_bool(cache_hit)}`"
            )
        issue_comment_lines.extend(issue_summary)

    issue_comment_lines.extend(
        [
            "",
            "Timing artifacts are attached for each seed, cold, and warm child job as `cache-benchmark-<backend>-<mutation>-<stage>-timings`.",
        ]
    )

    return issue_comment_lines


def _build_phase1_workflow_detail_lines(
    config: dict[str, Any], payload: dict[str, Any]
) -> list[str]:
    mutation_by_id = _mutation_by_id(config)
    detail_lines: list[str] = []

    for result in payload["results"]:
        mutation_id = result["mutation"]
        label = _phase1_result_label(mutation_by_id, mutation_id)
        status = result.get("result", "success")
        if status == "skipped":
            continue

        cold = _read_float(result.get("cold_seconds"))
        warm = _read_float(result.get("warm_seconds"))
        saved = _read_float(result.get("saved_seconds"))
        ratio = _read_float(result.get("speedup_ratio"))
        cache_hit = _read_bool(result.get("cache_hit"))
        hit_detail = result.get("cache_hit_detail") or "n/a"
        detail_lines.extend(
            [
                f"#### {label}",
                "",
                f"- job result: `{status}`",
                f"- cold wall seconds: `{_format_seconds(cold)}`",
                f"- warm wall seconds: `{_format_seconds(warm)}`",
                f"- seconds saved: `{_format_seconds(saved)}`",
                f"- speedup ratio: `{_format_ratio(ratio)}`",
                f"- warm cache hit: `{_format_bool(cache_hit)}`",
                f"- warm cache hit detail: `{hit_detail}`",
                "",
            ]
        )

    return detail_lines


def _write_phase1_issue_comment(lines: list[str]) -> str | None:
    issue_comment_path = os.environ.get("BENCHMARK_PHASE1_ISSUE_COMMENT_PATH")
    if not issue_comment_path:
        return None
    output_path = Path(issue_comment_path)
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return issue_comment_path


def _build_phase1_summary_lines(config: dict[str, Any], payload: dict[str, Any]) -> list[str]:
    phase1 = config["phase1"]
    runner = payload.get("runner") or phase1["runner"]
    target = payload.get("target") or phase1["target"]
    threshold = float(payload.get("threshold_ratio") or phase1["default_threshold_ratio"])
    cache_backend = payload["cache_backend"]
    scenario = payload["scenario"]
    command = _format_command(phase1["command"], target)
    issue_comment_lines = _build_phase1_issue_comment_lines(config, payload)
    issue_target = _phase1_issue_target(config)
    issue_comment_path = _write_phase1_issue_comment(issue_comment_lines)
    workflow_summary = [
        "### Cache Benchmark Summary",
        "",
        f"- cache backend: `{cache_backend}`",
        f"- requested scenario: `{scenario}`",
        f"- required ratio: `{threshold:.2f}x`",
        f"- runner: `{runner}`",
        f"- target: `{target}`",
        f"- measured command: `{command}`",
        "",
    ]
    workflow_summary.extend(_build_phase1_workflow_detail_lines(config, payload))
    if issue_comment_path:
        workflow_summary.extend(
            [
                "### Issue Comment Artifact",
                "",
                f"- markdown artifact: `{issue_comment_path}`",
                "",
            ]
        )

    return workflow_summary + [
        "### Issue Comment Draft",
        "",
        "```markdown",
        *issue_comment_lines,
        "```",
        "",
        f"Copy this block into issue {issue_target}.",
    ]


def _append_phase1_step_summary(config: dict[str, Any]) -> None:
    input_path = Path(os.environ["BENCHMARK_PHASE1_INPUT_JSON"])
    payload = json.loads(input_path.read_text(encoding="utf-8"))
    summary_path = os.environ.get("GITHUB_STEP_SUMMARY")
    if not summary_path:
        return
    summary_lines = _build_phase1_summary_lines(config, payload)
    with Path(summary_path).open("a", encoding="utf-8") as handle:
        handle.write("\n".join(summary_lines) + "\n")


def main() -> None:
    config, config_path = _load_config()
    if os.environ.get("BENCHMARK_REPORT_MODE") == "phase1-summary":
        _append_phase1_step_summary(config)
        return
    results, competitors, profiles, mutations = _load_results(config)
    report = _build_report(config, config_path, results, competitors, profiles, mutations)
    _write_json_report(report)
    _write_www_bundle(report)
    _append_step_summary(report)


if __name__ == "__main__":
    main()
