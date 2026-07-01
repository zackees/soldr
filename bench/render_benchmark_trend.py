#!/usr/bin/env python3
"""Render the benchmark-stats history.jsonl as a static PNG trend chart
that the repo README can embed via raw.githubusercontent.com.

Issue #771. Mirrors zccache's `## Performance` README pattern.

Reads:  ./benchmark-stats/history.jsonl
Writes: ./benchmark-stats/benchmark-trend.png

stdlib + matplotlib only. matplotlib is installed in the workflow via
`uv pip install --system --break-system-packages matplotlib Pillow`
(PyPI wheels — see soldr#1166 for why we no longer apt-install it).
"""

import json
import sys
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402

# Match bench/index.html palette so README image and Pages view are
# visually consistent.
CANARY_COLORS = {
    "cargo-build-medium-cold": "#d32f2f",
    "cargo-build-medium-warm": "#1976d2",
    "cargo-build-medium-from-warm-zccache": "#388e3c",
    "cargo-check-medium-cross-verb": "#f57c00",
    "touch-no-change-medium-warm": "#7b1fa2",
    "worktree-share-medium-warm": "#00838f",
}

REPO_ROOT = Path(__file__).resolve().parent.parent
HISTORY = REPO_ROOT / "benchmark-stats" / "history.jsonl"
OUTPUT = REPO_ROOT / "benchmark-stats" / "benchmark-trend.png"


def load_history():
    if not HISTORY.exists():
        return []
    rows = []
    with HISTORY.open("r", encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                rows.append(json.loads(line))
            except json.JSONDecodeError as err:
                print(
                    f"render: skipping malformed history.jsonl line: {err}",
                    file=sys.stderr,
                )
    return rows


def render_empty(reason):
    fig, ax = plt.subplots(figsize=(8, 4.5))
    ax.set_axis_off()
    ax.text(
        0.5,
        0.5,
        f"No benchmark data yet\n({reason})",
        ha="center",
        va="center",
        fontsize=14,
        color="#656d76",
        transform=ax.transAxes,
    )
    fig.savefig(OUTPUT, dpi=110, bbox_inches="tight")
    plt.close(fig)


def render_trend(rows):
    canary_names = list(CANARY_COLORS.keys())
    x = list(range(len(rows)))  # commit index; oldest on left, newest on right
    fig, ax = plt.subplots(figsize=(8, 4.5))
    for name in canary_names:
        # 0 ms means the canary failed (run_canaries.sh's defensive
        # fallback). Map to None so it shows as a gap on the log axis
        # rather than as a matplotlib log-error / misleading floor.
        ys = [(row.get("canaries", {}).get(name) or None) for row in rows]
        ax.plot(
            x,
            ys,
            marker="o",
            markersize=2,
            linewidth=1.2,
            color=CANARY_COLORS[name],
            label=name,
        )
    ax.set_yscale("log")
    ax.set_xlabel("main-commit index (older → newer)")
    ax.set_ylabel("wall time (ms, log scale)")
    ax.set_title(f"soldr local-build canaries — last {len(rows)} main-commits")
    ax.grid(True, which="major", linestyle="-", linewidth=0.4, alpha=0.5)
    ax.grid(True, which="minor", linestyle=":", linewidth=0.3, alpha=0.4)
    ax.legend(loc="upper left", fontsize=8, frameon=True, ncol=2)
    fig.tight_layout()
    fig.savefig(OUTPUT, dpi=110)
    plt.close(fig)


def main():
    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    rows = load_history()
    if not rows:
        render_empty("history.jsonl missing or empty")
    else:
        render_trend(rows)
    print(f"render: wrote {OUTPUT} ({len(rows)} rows)", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
