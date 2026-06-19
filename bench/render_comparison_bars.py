#!/usr/bin/env python3
"""Render README-facing comparison bar charts from comparison.json.

Reads:  ./benchmark-output/comparison.json
Writes: ./benchmark-stats/benchmark-rust-only.png
        ./benchmark-stats/benchmark-rust-c.png
"""

import json
import math
import sys
from collections import defaultdict
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402

REPO_ROOT = Path(__file__).resolve().parent.parent
INPUT = REPO_ROOT / "benchmark-output" / "comparison.json"
OUT_DIR = REPO_ROOT / "benchmark-stats"

TOOL_COLORS = {
    "bare": "#d32f2f",
    "sccache": "#1976d2",
    "soldr": "#2da44e",
}

BENCHMARKS = {
    "rust-only": {
        "title": "soldr vs sccache vs bare cargo - Rust workload",
        "output": "benchmark-rust-only.png",
    },
    "rust-c": {
        "title": "soldr vs sccache vs bare cargo - Rust+C workload",
        "output": "benchmark-rust-c.png",
    },
}

SCENARIO_ORDER = ["cold", "warm", "worktree-share"]
TOOL_ORDER = ["bare", "sccache", "soldr"]


def load_comparison():
    with INPUT.open("r", encoding="utf-8-sig") as f:
        return json.load(f)


def by_benchmark(rows):
    grouped = defaultdict(dict)
    for row in rows:
        key = (row.get("benchmark"), row.get("scenario_key"), row.get("tool"))
        grouped[key] = row
    return grouped


def positive_or_none(value):
    if isinstance(value, (int, float)) and value > 0:
        return value
    return None


def ratio_text(sccache_ms, soldr_ms):
    sccache_ms = positive_or_none(sccache_ms)
    soldr_ms = positive_or_none(soldr_ms)
    if not sccache_ms or not soldr_ms:
        return None
    ratio = sccache_ms / soldr_ms
    if ratio < 1:
        return f"{1 / ratio:.1f}x slower"
    if ratio >= 10:
        return f"{ratio:.0f}x faster"
    return f"{ratio:.1f}x faster"


def render_chart(doc, benchmark, grouped):
    meta = BENCHMARKS[benchmark]
    scenarios = {item["key"]: item["label"] for item in doc.get("scenarios", [])}
    tools = {item["key"]: item["label"] for item in doc.get("tools", [])}

    fig, ax = plt.subplots(figsize=(9.2, 4.8))
    group_width = 0.72
    bar_width = group_width / len(TOOL_ORDER)
    x_positions = list(range(len(SCENARIO_ORDER)))

    max_y = 1
    for tool_idx, tool in enumerate(TOOL_ORDER):
        xs = [x - group_width / 2 + bar_width / 2 + tool_idx * bar_width for x in x_positions]
        ys = []
        for scenario in SCENARIO_ORDER:
            row = grouped.get((benchmark, scenario, tool), {})
            value = positive_or_none(row.get("wall_ms"))
            ys.append(value)
            if value:
                max_y = max(max_y, value)
        ax.bar(
            xs,
            [value or math.nan for value in ys],
            width=bar_width * 0.92,
            label=tools.get(tool, tool),
            color=TOOL_COLORS.get(tool, "#656d76"),
        )

    for idx, scenario in enumerate(SCENARIO_ORDER):
        sccache = grouped.get((benchmark, scenario, "sccache"), {}).get("wall_ms")
        soldr = grouped.get((benchmark, scenario, "soldr"), {}).get("wall_ms")
        label = ratio_text(sccache, soldr)
        soldr_value = positive_or_none(soldr)
        if label and soldr_value:
            soldr_x = idx - group_width / 2 + bar_width / 2 + TOOL_ORDER.index("soldr") * bar_width
            ax.annotate(
                label,
                xy=(soldr_x, soldr_value),
                xytext=(0, 7),
                textcoords="offset points",
                ha="center",
                va="bottom",
                fontsize=8,
                color=TOOL_COLORS["soldr"],
                fontweight="bold",
            )

    ax.set_yscale("log")
    ax.set_ylabel("wall time (ms, log scale)")
    ax.set_title(meta["title"])
    ax.set_xticks(x_positions)
    ax.set_xticklabels([scenarios.get(key, key) for key in SCENARIO_ORDER], rotation=8, ha="right")
    ax.grid(True, which="major", axis="y", linestyle="-", linewidth=0.4, alpha=0.45)
    ax.grid(True, which="minor", axis="y", linestyle=":", linewidth=0.3, alpha=0.35)
    ax.legend(loc="upper right", fontsize=9)
    ax.set_ylim(bottom=1, top=max_y * 5)
    fig.tight_layout()
    output = OUT_DIR / meta["output"]
    fig.savefig(output, dpi=130)
    plt.close(fig)
    print(f"render: wrote {output}", file=sys.stderr)


def main():
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    if not INPUT.exists():
        print(f"render: missing {INPUT}", file=sys.stderr)
        return 1
    doc = load_comparison()
    grouped = by_benchmark(doc.get("results", []))
    for benchmark in BENCHMARKS:
        render_chart(doc, benchmark, grouped)
    return 0


if __name__ == "__main__":
    sys.exit(main())
