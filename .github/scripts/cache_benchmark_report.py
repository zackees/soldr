#!/usr/bin/env python3
"""Generate the cache benchmark report and rendered site bundle."""

from __future__ import annotations

import json
import math
import os
import tomllib
from collections import defaultdict
from datetime import datetime, timezone
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


def _read_int(value: Any) -> int | None:
    if value in ("", None):
        return None
    return int(value)


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


def _compute_cross_pr_speedup(
    comparison_rows: list[dict[str, Any]], base_competitor_id: str
) -> dict[str, float] | None:
    """Aggregate cross-PR speedup across every row whose backends both
    populated `cross_pr_build_seconds`.

    Issue #650. Returns `{wall, mean, min, max}` so the headline can
    surface the full picture:

    - `wall` — sum(base) / sum(soldr). What the CI total wall time looks
      like if you ran every measured cell back-to-back. Cheap cells
      contribute less because they take less wall time.
    - `mean` — mean of per-row ratios. \"On a uniformly random cell, how
      much faster is soldr.\"
    - `min` / `max` — the per-cell extremes. Expensive cells (release)
      undershoot 2x; cheap cells (quick / lint) clear 2x easily.

    Returns None when no row has both backends populated.
    """
    s_total = 0.0
    b_total = 0.0
    ratios: list[float] = []
    for row in comparison_rows:
        soldr = (row.get("competitors", {}) or {}).get("soldr") or {}
        base = (row.get("competitors", {}) or {}).get(base_competitor_id) or {}
        s = soldr.get("cross_pr_build_seconds")
        b = base.get("cross_pr_build_seconds")
        if isinstance(s, (int, float)) and isinstance(b, (int, float)) and s > 0:
            s_total += s
            b_total += b
            ratios.append(b / s)
    if not ratios or s_total <= 0:
        return None
    return {
        "wall": b_total / s_total,
        "mean": sum(ratios) / len(ratios),
        "min": min(ratios),
        "max": max(ratios),
    }


def _compute_cache_ratio_pct(
    comparison_rows: list[dict[str, Any]], base_competitor_id: str
) -> float | None:
    """Median soldr cache size / swatinem cache size across successful rows.

    Issue #639. Headline-level summary: if soldr's cache is consistently
    smaller than swatinem's across the configured cells, the published
    page should say so. Median (not mean) so a single outlier row from
    a profile mismatch doesn't drag the number.
    """
    ratios: list[float] = []
    for row in comparison_rows:
        soldr = row.get("competitors", {}).get("soldr") or {}
        base = row.get("competitors", {}).get(base_competitor_id) or {}
        s_bytes = soldr.get("cache_dir_bytes")
        b_bytes = base.get("cache_dir_bytes")
        if not s_bytes or not b_bytes:
            continue
        ratios.append(100.0 * s_bytes / b_bytes)
    if not ratios:
        return None
    ratios.sort()
    n = len(ratios)
    if n % 2 == 1:
        return ratios[n // 2]
    return 0.5 * (ratios[n // 2 - 1] + ratios[n // 2])


def _format_bytes(value: int | None) -> str:
    """Compact human-readable size. Powers of 1024 with two-decimal MiB / GiB.

    Issue #639: the 8 GB cache-size budget the project tracks against
    real-world workspaces (fastled/fbuild) needs to be visible in the
    rendered headline alongside the speedup, so the report uses GiB above
    1 GiB and MiB below — same convention the GitHub Actions cache UI uses.
    """
    if value is None:
        return "n/a"
    if value < 1024 * 1024:
        return f"{value / 1024:.1f} KiB"
    if value < 1024 * 1024 * 1024:
        return f"{value / (1024 * 1024):.1f} MiB"
    return f"{value / (1024 * 1024 * 1024):.2f} GiB"


def _format_bool(value: bool | None) -> str:
    if value is None:
        return "n/a"
    return "true" if value else "false"


def _generated_at_utc() -> str:
    override = os.environ.get("BENCHMARK_GENERATED_AT_UTC")
    if override:
        return override
    return (
        datetime.now(timezone.utc)
        .replace(microsecond=0)
        .isoformat()
        .replace("+00:00", "Z")
    )


def _human_datetime_utc(value: str) -> str:
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return value
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=timezone.utc)
    parsed = parsed.astimezone(timezone.utc)
    hour = parsed.strftime("%I").lstrip("0") or "0"
    return f"{parsed.strftime('%B')} {parsed.day}, {parsed.year} at {hour}:{parsed:%M} {parsed:%p} UTC"


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
) -> tuple[
    list[dict[str, Any]],
    list[dict[str, Any]],
    list[dict[str, Any]],
    list[dict[str, Any]],
    list[dict[str, Any]],
]:
    input_path = Path(os.environ["BENCHMARK_INPUT_JSON"])
    payload = json.loads(input_path.read_text(encoding="utf-8"))
    raw_results = payload["results"] if isinstance(payload, dict) else payload
    native_sqlite_results = (
        _load_native_sqlite_results(config, payload) if isinstance(payload, dict) else []
    )

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
                "cache_dir_bytes": _read_int(raw_result.get("cache_dir_bytes")),
                "archive_seconds": _read_float(raw_result.get("archive_seconds")),
                "archive_bytes": _read_int(raw_result.get("archive_bytes")),
                "restore_seconds": _read_float(raw_result.get("restore_seconds")),
                "restored_warm_seconds": _read_float(raw_result.get("restored_warm_seconds")),
                "cross_pr_seed_seconds": _read_float(raw_result.get("cross_pr_seed_seconds")),
                "cross_pr_build_seconds": _read_float(raw_result.get("cross_pr_build_seconds")),
                "threshold_failed": bool(raw_result.get("threshold_failed", False)),
            }
        )

    return results, native_sqlite_results, competitors, profiles, mutations


def _raw_native_sqlite_rows(payload: dict[str, Any]) -> list[dict[str, Any]]:
    rows = payload.get("native_sqlite", [])
    if isinstance(rows, dict):
        rows = rows.get("results", [rows])
    if not isinstance(rows, list):
        raise ValueError("native_sqlite must be a list, object, or object with results")
    if not all(isinstance(row, dict) for row in rows):
        raise ValueError("native_sqlite rows must be objects")
    return rows


def _load_native_sqlite_results(
    config: dict[str, Any], payload: dict[str, Any]
) -> list[dict[str, Any]]:
    native_config = config.get("native_sqlite", {})
    command_template = native_config.get("command")
    results: list[dict[str, Any]] = []

    for raw_result in _raw_native_sqlite_rows(payload):
        target = str(raw_result.get("target") or "unknown")
        command = raw_result.get("command")
        if not command and command_template:
            command = _format_command(command_template, target)
        results.append(
            {
                "target": target,
                "runner": raw_result.get("runner") or "unknown",
                "os": raw_result.get("os") or "unknown",
                "arch": raw_result.get("arch") or "unknown",
                "policy": raw_result.get("policy") or "unknown",
                "command": command,
                "result": raw_result.get("result", "success"),
                "cold_seconds": _round_metric(_read_float(raw_result.get("cold_seconds"))),
                "seed_seconds": _round_metric(_read_float(raw_result.get("seed_seconds"))),
                "warm_seconds": _round_metric(_read_float(raw_result.get("warm_seconds"))),
                "speedup_ratio": _round_metric(_read_float(raw_result.get("speedup_ratio"))),
                "cache_hit_detail": raw_result.get("cache_hit_detail") or None,
                "zccache_stats": raw_result.get("zccache_stats") or {},
            }
        )

    return results


def _build_report(
    config: dict[str, Any],
    config_path: Path,
    results: list[dict[str, Any]],
    native_sqlite_results: list[dict[str, Any]],
    competitors: list[dict[str, Any]],
    profiles: list[dict[str, Any]],
    mutations: list[dict[str, Any]],
) -> dict[str, Any]:
    site = config["site"]
    target = os.environ.get("BENCHMARK_COMMAND_TARGET") or site.get(
        "default_target", DEFAULT_TARGET
    )
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
        # Issue #639: same-job-seed warm timing is almost-tied; the real
        # advantage soldr ships in CI is a substantially smaller on-disk
        # cache (~half swatinem's in this workspace, since #640 wired the
        # measurement). Surface that ratio in the headline so the rendered
        # page doesn't read like "soldr is X% slower" without context.
        cache_ratio_pct = _compute_cache_ratio_pct(comparison_rows, base_competitor_id)
        cache_clause = ""
        if cache_ratio_pct is not None and cache_ratio_pct < 95:
            cache_clause = (
                f"; soldr's cache is {cache_ratio_pct:.0f}% the size of swatinem's"
            )
        # Issue #650: when the operator dispatches the workflow with
        # `include_cross_pr=true`, the per-row `cross_pr_build_seconds`
        # values carry the structural-advantage story — swatinem cannot
        # share artifacts between two PRs that touch different files,
        # soldr's content-addressed cache can. Surface the overall
        # speedup (sum/sum across rows where both backends produced a
        # number) so the headline reflects the actual measured win when
        # the scenario was exercised. Clause stays absent on scheduled
        # runs that don't populate the fields.
        cross_pr_speedup = _compute_cross_pr_speedup(
            comparison_rows, base_competitor_id
        )
        cross_pr_clause = ""
        if cross_pr_speedup is not None and cross_pr_speedup["max"] >= 1.10:
            # Use the range to capture both the expensive-cell floor and
            # the cheap-cell ceiling. Single-number aggregates hide that
            # cheap profiles routinely clear 2x while release builds drag
            # the average down to ~1.55x. Mean reports the typical-cell
            # win for a headline reader.
            cross_pr_clause = (
                f"; in the cross-PR cache-sharing scenario, soldr is "
                f"{cross_pr_speedup['min']:.1f}×-{cross_pr_speedup['max']:.1f}× "
                f"faster than swatinem (mean {cross_pr_speedup['mean']:.2f}×)"
            )
        headline = (
            f"Across {len(comparison_values)} configured comparisons, soldr is "
            f"{abs(average):.2f}% {trend} on warm time than swatinem and leads "
            f"{soldr_wins} rows{cache_clause}{cross_pr_clause}."
        )

    profile_commands = [
        {
            "id": profile["id"],
            "label": profile["label"],
            "command": _format_command(
                profile["command"],
                target,
            ),
        }
        for profile in profiles
    ]

    generated_at_utc = _generated_at_utc()
    report = {
        "workflow": "cache-benchmark.yml",
        "config_path": str(config_path.relative_to(REPO_ROOT)),
        "requested_scenario": os.environ["SCENARIO"],
        "threshold_ratio": _round_metric(float(os.environ["THRESHOLD_RATIO"])),
        "headline": headline,
        "metadata": {
            "generated_at_utc": generated_at_utc,
            "last_executed_at_human": _human_datetime_utc(generated_at_utc),
            "git_sha": os.environ.get("GITHUB_SHA") or "unknown",
            "github_run_id": os.environ.get("GITHUB_RUN_ID") or "local",
            "runner_os": os.environ.get("RUNNER_OS") or "unknown",
            "runner_arch": os.environ.get("RUNNER_ARCH") or "unknown",
            "target": target,
        },
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
    if native_sqlite_results:
        native_config = config.get("native_sqlite", {})
        report["native_sqlite"] = {
            "issue": native_config.get("issue"),
            "label": native_config.get("label", "Native SQLite"),
            "mode": native_config.get("mode", "report-only"),
            "fixture_manifest": native_config.get("fixture_manifest"),
            "command": native_config.get("command"),
            "policies": native_config.get("policies", {}),
            "targets": native_config.get("targets", []),
            "results": native_sqlite_results,
        }

    return report


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
            f"<td>{_format_bytes(soldr.get('cache_dir_bytes'))}</td>"
            f"<td>{_format_seconds(swatinem.get('cold_seconds'))}</td>"
            f"<td>{_format_seconds(swatinem.get('warm_seconds'))}</td>"
            f"<td>{_format_ratio(swatinem.get('speedup_ratio'))}</td>"
            f"<td>{_format_bytes(swatinem.get('cache_dir_bytes'))}</td>"
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


def _native_sqlite_results(report: dict[str, Any]) -> list[dict[str, Any]]:
    native = report.get("native_sqlite")
    if not native:
        return []
    return native.get("results", [])


def _build_native_sqlite_table_rows(report: dict[str, Any]) -> str:
    rows: list[str] = []
    for result in _native_sqlite_results(report):
        rows.append(
            "<tr>"
            f"<td><code>{escape(result['target'])}</code></td>"
            f"<td>{escape(result['runner'])}</td>"
            f"<td>{escape(result['policy'])}</td>"
            f"<td>{escape(result['result'])}</td>"
            f"<td>{_format_seconds(result.get('cold_seconds'))}</td>"
            f"<td>{_format_seconds(result.get('seed_seconds'))}</td>"
            f"<td>{_format_seconds(result.get('warm_seconds'))}</td>"
            f"<td>{_format_ratio(result.get('speedup_ratio'))}</td>"
            "</tr>"
        )
    return "\n".join(rows)


def _restore_phase_rows(report: dict[str, Any]) -> list[tuple[dict[str, Any], dict[str, Any], dict[str, Any]]]:
    """Yield (row, soldr_result, swatinem_result) when restore-phase data exists.

    Issue #639: PR #644 added `archive_*` / `restore_*` / `restored_warm_*`
    fields per row when the operator dispatches the workflow with
    `include_restore_phase=true`. The section only renders when *any* row
    has the data, so the scheduled-run page stays unchanged.
    """
    base_competitor_id = report["site"]["base_competitor"]
    out: list[tuple[dict[str, Any], dict[str, Any], dict[str, Any]]] = []
    for row in report["comparisons"]:
        soldr = _comparison_result(row, "soldr") or {}
        base = _comparison_result(row, base_competitor_id) or {}
        if any(
            soldr.get(key) is not None or base.get(key) is not None
            for key in ("archive_seconds", "archive_bytes", "restore_seconds", "restored_warm_seconds")
        ):
            out.append((row, soldr, base))
    return out


def _build_restore_phase_section(report: dict[str, Any]) -> str:
    rows_with_data = _restore_phase_rows(report)
    if not rows_with_data:
        return ""
    table_rows: list[str] = []
    for row, soldr, base in rows_with_data:
        table_rows.append(
            "<tr>"
            f"<td>{escape(row['profile_label'])}</td>"
            f"<td>{escape(row['mutation_label'])}</td>"
            f"<td>{_format_bytes(soldr.get('archive_bytes'))}</td>"
            f"<td>{_format_seconds(soldr.get('archive_seconds'))}</td>"
            f"<td>{_format_seconds(soldr.get('restore_seconds'))}</td>"
            f"<td>{_format_seconds(soldr.get('restored_warm_seconds'))}</td>"
            f"<td>{_format_bytes(base.get('archive_bytes'))}</td>"
            f"<td>{_format_seconds(base.get('archive_seconds'))}</td>"
            f"<td>{_format_seconds(base.get('restore_seconds'))}</td>"
            f"<td>{_format_seconds(base.get('restored_warm_seconds'))}</td>"
            "</tr>"
        )
    return f"""
      <h2>Cross-job restore</h2>
      <p class="meta">
        Opt-in measurement (workflow_dispatch input
        <code>include_restore_phase=true</code>). Snapshots the backend's
        cache to a <code>tar.gz</code>, wipes the workspace, untars,
        applies the mutation, and runs cargo again. Local untar is a
        <strong>lower bound</strong> on GHA cache restore (no network
        round-trip); the size + restore-time pair extrapolates the actual
        CI cost. Tracking under
        <a href="https://github.com/zackees/soldr/issues/639">soldr#639</a>.
      </p>
      <div class="table-wrap">
        <table>
          <thead>
            <tr>
              <th>Profile</th>
              <th>Change</th>
              <th>soldr archive</th>
              <th>soldr tar</th>
              <th>soldr untar</th>
              <th>soldr warm</th>
              <th>swatinem archive</th>
              <th>swatinem tar</th>
              <th>swatinem untar</th>
              <th>swatinem warm</th>
            </tr>
          </thead>
          <tbody>
            {chr(10).join(table_rows)}
          </tbody>
        </table>
      </div>
"""


def _cross_pr_rows(report: dict[str, Any]) -> list[tuple[dict[str, Any], dict[str, Any], dict[str, Any]]]:
    """Yield (row, soldr_result, swatinem_result) when cross-PR data exists.

    Issue #650: only renders when at least one row has the cross-PR fields
    populated, so scheduled runs are unaffected.
    """
    base_competitor_id = report["site"]["base_competitor"]
    out: list[tuple[dict[str, Any], dict[str, Any], dict[str, Any]]] = []
    for row in report["comparisons"]:
        soldr = _comparison_result(row, "soldr") or {}
        base = _comparison_result(row, base_competitor_id) or {}
        if any(
            soldr.get(key) is not None or base.get(key) is not None
            for key in ("cross_pr_seed_seconds", "cross_pr_build_seconds")
        ):
            out.append((row, soldr, base))
    return out


def _build_cross_pr_section(report: dict[str, Any]) -> str:
    rows = _cross_pr_rows(report)
    if not rows:
        return ""
    table_rows: list[str] = []
    for row, soldr, base in rows:
        s_seed = soldr.get("cross_pr_seed_seconds")
        s_build = soldr.get("cross_pr_build_seconds")
        b_seed = base.get("cross_pr_seed_seconds")
        b_build = base.get("cross_pr_build_seconds")
        speedup = (
            f"{b_build / s_build:.2f}x"
            if s_build and b_build and s_build > 0 and b_build > 0
            else "n/a"
        )
        table_rows.append(
            "<tr>"
            f"<td>{escape(row['profile_label'])}</td>"
            f"<td>{escape(row['mutation_label'])}</td>"
            f"<td>{_format_seconds(s_seed)}</td>"
            f"<td>{_format_seconds(s_build)}</td>"
            f"<td>{_format_seconds(b_seed)}</td>"
            f"<td>{_format_seconds(b_build)}</td>"
            f"<td>{escape(speedup)}</td>"
            "</tr>"
        )
    return f"""
      <h2>Cross-PR cache sharing</h2>
      <p class="meta">
        Opt-in measurement (workflow_dispatch input
        <code>include_cross_pr=true</code>). Seeds the backend's cache
        with mutation A applied (touch
        <code>crates/soldr-cli/src/fetch/github.rs</code>), switches to
        mutation B (touch <code>crates/soldr-cli/src/core/git.rs</code>,
        a deep module in a different subtree), wipes <code>target/</code>
        so cargo invokes rustc per unit, and times the rebuild. swatinem
        has no cross-PR cache share so this is effectively a cold rebuild
        for it; soldr's content-addressed cache serves hits for every
        unit whose inputs are unchanged across mutations. Tracking under
        <a href="https://github.com/zackees/soldr/issues/650">soldr#650</a>.
      </p>
      <div class="table-wrap">
        <table>
          <thead>
            <tr>
              <th>Profile</th>
              <th>Change</th>
              <th>soldr seed (A)</th>
              <th>soldr build (B)</th>
              <th>swatinem seed (A)</th>
              <th>swatinem build (B)</th>
              <th>soldr speedup vs swatinem</th>
            </tr>
          </thead>
          <tbody>
            {chr(10).join(table_rows)}
          </tbody>
        </table>
      </div>
"""


def _build_native_sqlite_section(report: dict[str, Any]) -> str:
    native = report.get("native_sqlite")
    if not native or not _native_sqlite_results(report):
        return ""
    issue = native.get("issue")
    issue_text = f"issue #{issue}" if issue is not None else "native cache tracking"
    mode = native.get("mode", "report-only")
    return f"""
      <h2>Native SQLite</h2>
      <p class="meta">
        Report-only bundled SQLite native C benchmark for {escape(issue_text)}.
        Mode: {escape(mode)}.
      </p>
      <div class="table-wrap">
        <table>
          <thead>
            <tr>
              <th>Target</th>
              <th>Runner</th>
              <th>Policy</th>
              <th>Result</th>
              <th>Cold</th>
              <th>Seed</th>
              <th>Warm</th>
              <th>Speedup</th>
            </tr>
          </thead>
          <tbody>
            {_build_native_sqlite_table_rows(report)}
          </tbody>
        </table>
      </div>
"""


def _metadata_line(report: dict[str, Any]) -> str:
    metadata = report["metadata"]
    git_sha = metadata["git_sha"]
    short_sha = git_sha[:12] if git_sha != "unknown" else git_sha
    last_executed = metadata.get("last_executed_at_human") or metadata["generated_at_utc"]
    return (
        f"Last run {last_executed} | "
        f"SHA {short_sha} | "
        f"{metadata['runner_os']}/{metadata['runner_arch']} | "
        f"Target {metadata['target']}"
    )


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
      <p class="meta">{escape(_metadata_line(report))}</p>
      <p class="note">
        {escape(soldr_note)} Raw detail is published beside this page as
        <a href="latest.json">latest.json</a>.
      </p>
      <p class="note">
        <strong>What "warm" measures here:</strong> every cell runs cold &rarr;
        warm inside the same CI job, so the warm pass already has a hot
        <code>target/</code> from the cold seed. swatinem's strength
        (multi-GB <code>target/</code> restore from a prior job) and soldr's
        strength (on-demand artifact fetch from a shared
        <a href="https://github.com/zackees/zccache">zccache</a>) are
        therefore both invisible &mdash; only the per-rustc wrapper overhead
        on the few units cargo's mtime fingerprint flags as dirty is on the
        clock. Treat the headline as a same-job-seed measurement, not a
        general &ldquo;A is faster than B&rdquo; claim. Tracking under
        <a href="https://github.com/zackees/soldr/issues/633">soldr#633</a>.
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
              <th>soldr cache</th>
              <th>swatinem cold</th>
              <th>swatinem warm</th>
              <th>swatinem speedup</th>
              <th>swatinem cache</th>
              <th>soldr vs swatinem</th>
            </tr>
          </thead>
          <tbody>
            {_build_table_rows(report)}
          </tbody>
        </table>
      </div>
      {_build_restore_phase_section(report)}
      {_build_cross_pr_section(report)}
      {_build_native_sqlite_section(report)}
      <h2>Benchmarked Commands</h2>
      <ul>
        {_build_profile_command_items(report)}
      </ul>
      <p class="footer">Config: <code>{escape(report["config_path"])}</code>.</p>
    </main>
  </body>
</html>
"""


def _load_image_font(size: int, bold: bool = False) -> Any:
    try:
        from PIL import ImageFont
    except ImportError as exc:
        raise SystemExit(
            "Pillow is required to generate benchmark.jpg. Install it with `python -m pip install pillow`."
        ) from exc

    names = (
        ["DejaVuSans-Bold.ttf", "Arial Bold.ttf"]
        if bold
        else ["DejaVuSans.ttf", "Arial.ttf"]
    )
    paths = [Path("/usr/share/fonts/truetype/dejavu") / name for name in names] + [
        Path("C:/Windows/Fonts") / name for name in names
    ]
    for path in paths:
        if path.exists():
            try:
                return ImageFont.truetype(str(path), size)
            except OSError:
                continue
    return ImageFont.load_default()


def _text_width(draw: Any, text: str, font: Any) -> int:
    left, _top, right, _bottom = draw.textbbox((0, 0), text, font=font)
    return right - left


def _truncate_text(draw: Any, text: str, font: Any, max_width: int) -> str:
    if _text_width(draw, text, font) <= max_width:
        return text
    suffix = "..."
    trimmed = text
    while trimmed and _text_width(draw, trimmed + suffix, font) > max_width:
        trimmed = trimmed[:-1]
    return (trimmed + suffix) if trimmed else suffix


def _wrap_text(draw: Any, text: str, font: Any, max_width: int, max_lines: int) -> list[str]:
    words = text.split()
    lines: list[str] = []
    current = ""
    for word in words:
        candidate = word if not current else f"{current} {word}"
        if _text_width(draw, candidate, font) <= max_width:
            current = candidate
            continue
        if current:
            lines.append(current)
        current = word
        if len(lines) == max_lines:
            break
    if current and len(lines) < max_lines:
        lines.append(current)
    if len(lines) > max_lines:
        lines = lines[:max_lines]
    if len(lines) == max_lines:
        original_prefix = " ".join(lines)
        if len(original_prefix) < len(text):
            lines[-1] = _truncate_text(draw, lines[-1], font, max_width)
    return lines


def _base_competitor_label(report: dict[str, Any]) -> str:
    base = report["site"]["base_competitor"]
    for competitor in report["competitors"]:
        if competitor["id"] == base:
            return competitor["label"]
    return base


def _write_benchmark_image(report: dict[str, Any], output_path: Path) -> None:
    try:
        from PIL import Image, ImageDraw
    except ImportError as exc:
        raise SystemExit(
            "Pillow is required to generate benchmark.jpg. Install it with `python -m pip install pillow`."
        ) from exc

    width, height = 1200, 630
    image = Image.new("RGB", (width, height), "#f8f8f6")
    draw = ImageDraw.Draw(image)

    title_font = _load_image_font(48, bold=True)
    headline_font = _load_image_font(27, bold=True)
    body_font = _load_image_font(24)
    small_font = _load_image_font(19)
    table_font = _load_image_font(20)
    table_header_font = _load_image_font(20, bold=True)

    ink = "#202426"
    muted = "#4e5a5f"
    border = "#d7dcdf"
    panel = "#ffffff"
    accent = "#0f766e"
    x = 52
    y = 42

    draw.text((x, y), report["site"]["title"], fill=ink, font=title_font)
    y += 66
    for line in _wrap_text(draw, report["headline"], headline_font, width - 2 * x, 2):
        draw.text((x, y), line, fill=accent, font=headline_font)
        y += 36

    y += 12
    last_executed = (
        f"Last run {report['metadata'].get('last_executed_at_human', report['metadata']['generated_at_utc'])}"
    )
    draw.text(
        (x, y),
        _truncate_text(draw, last_executed, small_font, width - 2 * x),
        fill=muted,
        font=small_font,
    )
    y += 28

    metadata = (
        f"Scenario {report['requested_scenario']} | "
        f"Threshold {report['threshold_ratio']:.2f}x | "
        f"SHA {report['metadata']['git_sha'][:12] if report['metadata']['git_sha'] != 'unknown' else 'unknown'} | "
        f"{report['metadata']['runner_os']}/{report['metadata']['runner_arch']} | "
        f"Target {report['metadata']['target']}"
    )
    draw.text(
        (x, y),
        _truncate_text(draw, metadata, small_font, width - 2 * x),
        fill=muted,
        font=small_font,
    )
    y += 46

    table_x = x
    table_y = y
    table_w = width - 2 * x
    row_h = 46
    columns = [
        ("Profile", 205),
        ("Change", 220),
        ("soldr warm", 160),
        (f"{_base_competitor_label(report)} warm", 190),
        ("soldr vs base", 160),
    ]
    used_w = sum(col_width for _label, col_width in columns)
    columns[-1] = (columns[-1][0], columns[-1][1] + max(0, table_w - used_w))

    draw.rounded_rectangle(
        (table_x, table_y, table_x + table_w, table_y + row_h * 7),
        radius=8,
        fill=panel,
        outline=border,
        width=1,
    )
    draw.rectangle((table_x, table_y, table_x + table_w, table_y + row_h), fill="#eef2f3")

    cursor = table_x
    for label, col_w in columns:
        draw.text((cursor + 14, table_y + 13), label, fill=ink, font=table_header_font)
        cursor += col_w
        draw.line((cursor, table_y, cursor, table_y + row_h * 7), fill=border, width=1)
    draw.line((table_x, table_y + row_h, table_x + table_w, table_y + row_h), fill=border, width=1)

    rows = report["comparisons"][:6]
    base_id = report["site"]["base_competitor"]
    for idx, row in enumerate(rows):
        row_y = table_y + row_h * (idx + 1)
        if idx % 2 == 1:
            draw.rectangle((table_x, row_y, table_x + table_w, row_y + row_h), fill="#fafcfc")
        soldr = _comparison_result(row, "soldr") or {}
        base = _comparison_result(row, base_id) or {}
        values = [
            row["profile_label"],
            row["mutation_label"],
            _format_seconds(soldr.get("warm_seconds")),
            _format_seconds(base.get("warm_seconds")),
            _format_percent(row["soldr_vs_base_warm_percent"]),
        ]
        cursor = table_x
        for value, (_label, col_w) in zip(values, columns):
            text = _truncate_text(draw, value, table_font, col_w - 24)
            draw.text((cursor + 14, row_y + 13), text, fill=ink, font=table_font)
            cursor += col_w
        draw.line((table_x, row_y + row_h, table_x + table_w, row_y + row_h), fill=border, width=1)

    footer_y = height - 70
    footer = "Full report: https://zackees.github.io/soldr/ | Raw data: latest.json"
    draw.text((x, footer_y), footer, fill=muted, font=body_font)
    image.save(output_path, "JPEG", quality=90, optimize=True)


def _write_www_bundle(report: dict[str, Any]) -> None:
    www_dir = os.environ.get("BENCHMARK_SUMMARY_WWW_DIR")
    if not www_dir:
        return

    output_dir = Path(www_dir)
    output_dir.mkdir(parents=True, exist_ok=True)
    (output_dir / "index.html").write_text(_build_html_page(report), encoding="utf-8")
    (output_dir / "latest.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    _write_benchmark_image(report, output_dir / "benchmark.jpg")
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

    native_rows = _native_sqlite_results(report)
    if native_rows:
        lines.extend(
            [
                "",
                "### Native SQLite",
                "",
                "| target | runner | policy | result | cold | seed | warm | speedup |",
                "| --- | --- | --- | --- | ---: | ---: | ---: | ---: |",
            ]
        )
        for result in native_rows:
            lines.append(
                f"| `{result['target']}` | `{result['runner']}` | "
                f"`{result['policy']}` | `{result['result']}` | "
                f"`{_format_seconds(result.get('cold_seconds'))}` | "
                f"`{_format_seconds(result.get('seed_seconds'))}` | "
                f"`{_format_seconds(result.get('warm_seconds'))}` | "
                f"`{_format_ratio(result.get('speedup_ratio'))}` |"
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
    results, native_sqlite_results, competitors, profiles, mutations = _load_results(config)
    report = _build_report(
        config,
        config_path,
        results,
        native_sqlite_results,
        competitors,
        profiles,
        mutations,
    )
    _write_json_report(report)
    _write_www_bundle(report)
    _append_step_summary(report)


if __name__ == "__main__":
    main()
