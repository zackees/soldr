#!/usr/bin/env python3
"""Render zccache-style dark README comparison charts.

Reads:  ./benchmark-output/comparison.json
Writes: ./benchmark-stats/benchmark-rust-only.jpg
        ./benchmark-stats/benchmark-rust-c.jpg
"""

from __future__ import annotations

import json
import sys
from pathlib import Path
from typing import Any

try:
    from PIL import Image, ImageDraw, ImageFont
except ImportError as exc:  # pragma: no cover - exercised in workflow setup
    raise SystemExit(
        "Pillow is required to render benchmark JPGs. Install python3-pil."
    ) from exc


REPO_ROOT = Path(__file__).resolve().parent.parent
INPUT = REPO_ROOT / "benchmark-output" / "comparison.json"
OUT_DIR = REPO_ROOT / "benchmark-stats"

BENCHMARKS = {
    "rust-only": {
        "title": "soldr Rust benchmarks",
        "subtitle": "bare cargo vs sccache vs soldr",
        "output": "benchmark-rust-only.jpg",
    },
    "rust-c": {
        "title": "soldr Rust+C benchmarks",
        "subtitle": "rust-native fixture",
        "output": "benchmark-rust-c.jpg",
    },
}

OVERLAY_SECTIONS = [
    {
        "key": "warm",
        "label": "Warm rebuild (same workspace)",
        "cold_key": "cold",
        "warm_key": "warm",
    },
    {
        "key": "worktree-share",
        "label": "Agent worktree / parent-child share",
        "cold_key": "cold",
        "warm_key": "worktree-share",
    },
]
TOOL_ORDER = ["bare", "sccache", "soldr"]
TOOL_COLOR_PAIRS = {
    "bare": {"cold": "#3b4046", "warm": "#8b949e"},
    "sccache": {"cold": "#1f3a7a", "warm": "#79c0ff"},
    "soldr": {"cold": "#5b1f1c", "warm": "#f85149"},
}
WARM_VIOLATION_OUTLINE = "#ffd33d"


def load_comparison() -> dict[str, Any]:
    with INPUT.open("r", encoding="utf-8-sig") as f:
        return json.load(f)


def font(size: int, bold: bool = False) -> ImageFont.FreeTypeFont | ImageFont.ImageFont:
    names = (
        ("DejaVuSans-Bold.ttf", "Arial Bold.ttf") if bold else ("DejaVuSans.ttf", "Arial.ttf")
    )
    for name in names:
        try:
            return ImageFont.truetype(name, size)
        except OSError:
            pass
    return ImageFont.load_default()


def hex_rgb(value: str) -> tuple[int, int, int]:
    value = value.lstrip("#")
    return tuple(int(value[i : i + 2], 16) for i in (0, 2, 4))


def text_width(draw: ImageDraw.ImageDraw, value: str, fnt: Any) -> int:
    box = draw.textbbox((0, 0), value, font=fnt)
    return int(box[2] - box[0])


def truncate(draw: ImageDraw.ImageDraw, value: str, fnt: Any, max_width: int) -> str:
    if text_width(draw, value, fnt) <= max_width:
        return value
    suffix = "..."
    available = max(0, max_width - text_width(draw, suffix, fnt))
    out = ""
    for char in value:
        if text_width(draw, out + char, fnt) > available:
            break
        out += char
    return out.rstrip() + suffix


def seconds_label(ms: Any) -> str:
    if not isinstance(ms, (int, float)) or ms <= 0:
        return "n/a"
    seconds = ms / 1000.0
    if seconds < 1:
        return f"{ms:.0f}ms"
    return f"{seconds:.3f}s"


def bytes_label(value: Any) -> str:
    if not isinstance(value, (int, float)) or value < 0:
        return "n/a"
    units = ("B", "KiB", "MiB", "GiB")
    amount = float(value)
    unit = units[0]
    for unit in units:
        if amount < 1024 or unit == units[-1]:
            break
        amount /= 1024
    if unit == "B":
        return f"{int(amount)} B"
    if amount < 10:
        return f"{amount:.1f} {unit}"
    return f"{amount:.0f} {unit}"


def overlay_speedup_label(
    rows_by_key: dict[tuple[str, str, str], dict[str, Any]],
    benchmark: str,
    warm_key: str,
) -> str:
    sccache = rows_by_key.get((benchmark, warm_key, "sccache"), {}).get("wall_ms")
    soldr = rows_by_key.get((benchmark, warm_key, "soldr"), {}).get("wall_ms")
    if not isinstance(sccache, (int, float)) or not isinstance(soldr, (int, float)):
        return ""
    if sccache <= 0 or soldr <= 0:
        return ""
    ratio = sccache / soldr
    if ratio >= 1:
        return f"soldr warm {ratio:.1f}x faster than sccache"
    return f"soldr warm {(1 / ratio):.1f}x slower than sccache"


def cold_max_for_section(
    rows_by_key: dict[tuple[str, str, str], dict[str, Any]],
    benchmark: str,
    cold_key: str,
    warm_key: str,
) -> tuple[float, str]:
    cold_values = []
    for tool in TOOL_ORDER:
        value = rows_by_key.get((benchmark, cold_key, tool), {}).get("wall_ms")
        if isinstance(value, (int, float)) and value > 0:
            cold_values.append(float(value))
    if cold_values:
        return max(cold_values), "cold"

    warm_values = []
    for tool in TOOL_ORDER:
        value = rows_by_key.get((benchmark, warm_key, tool), {}).get("wall_ms")
        if isinstance(value, (int, float)) and value > 0:
            warm_values.append(float(value))
    if warm_values:
        return max(warm_values), "warm"
    return 1.0, "none"


def render_benchmark(doc: dict[str, Any], benchmark: str) -> Path:
    rows = [row for row in doc.get("results", []) if row.get("benchmark") == benchmark]
    rows_by_key = {
        (row.get("benchmark"), row.get("scenario_key"), row.get("tool")): row
        for row in rows
    }
    tool_labels = {item["key"]: item["label"] for item in doc.get("tools", [])}

    scale = 3
    width = 900
    margin = 20
    header_h = 116
    legend_h = 48
    section_h = 240
    section_gap = 16
    footer_h = 50
    height = header_h + legend_h + (section_h + section_gap) * len(OVERLAY_SECTIONS) - section_gap + footer_h

    image = Image.new("RGB", (width * scale, height * scale), hex_rgb("#0d1117"))
    draw = ImageDraw.Draw(image)

    title_font = font(32 * scale, bold=True)
    subtitle_font = font(14 * scale)
    scenario_font = font(20 * scale, bold=True)
    tool_font = font(16 * scale, bold=True)
    value_font = font(15 * scale, bold=True)
    small_font = font(13 * scale)

    def xy(x: int, y: int) -> tuple[int, int]:
        return x * scale, y * scale

    def rect(values: tuple[int, int, int, int], fill: str, outline: str | None = None, width_px: int = 1) -> None:
        draw.rectangle(
            tuple(v * scale for v in values),
            fill=hex_rgb(fill),
            outline=hex_rgb(outline) if outline else None,
            width=width_px * scale,
        )

    meta = BENCHMARKS[benchmark]
    rect((0, 0, width, header_h), "#161b22")
    draw.text(xy(margin, 22), meta["title"], font=title_font, fill=hex_rgb("#f0f6fc"))
    generated = doc.get("ran_at", "unknown")
    versions = " | ".join(
        part
        for part in (
            doc.get("soldr_version"),
            doc.get("sccache_version"),
            doc.get("rustc_version"),
        )
        if part
    )
    header_line = f"{meta['subtitle']} | generated {generated}"
    draw.text(xy(margin, 70), truncate(draw, header_line, subtitle_font, (width - margin * 2) * scale), font=subtitle_font, fill=hex_rgb("#8b949e"))
    draw.text(
        xy(margin, 92),
        truncate(
            draw,
            f"{versions} | cold bars in back; warm overlays in front",
            subtitle_font,
            (width - margin * 2) * scale,
        ),
        font=subtitle_font,
        fill=hex_rgb("#8b949e"),
    )

    legend_y = header_h + 10
    legend_x = margin
    rect((legend_x, legend_y + 2, legend_x + 58, legend_y + 22), TOOL_COLOR_PAIRS["soldr"]["cold"])
    rect((legend_x, legend_y + 7, legend_x + 26, legend_y + 17), TOOL_COLOR_PAIRS["soldr"]["warm"])
    draw.text(
        xy(legend_x + 70, legend_y + 1),
        "cold (back) + warm (front, overlays cold)",
        font=small_font,
        fill=hex_rgb("#c9d1d9"),
    )
    violation_x = width - margin - 236
    rect(
        (violation_x, legend_y + 7, violation_x + 34, legend_y + 17),
        TOOL_COLOR_PAIRS["soldr"]["warm"],
        outline=WARM_VIOLATION_OUTLINE,
        width_px=2,
    )
    draw.text(
        xy(violation_x + 44, legend_y + 1),
        "warm > cold",
        font=small_font,
        fill=hex_rgb(WARM_VIOLATION_OUTLINE),
    )

    chart_x = margin
    chart_w = width - margin * 2
    label_w = 150
    value_w = 184
    bar_x0 = chart_x + label_w
    bar_x1 = chart_x + chart_w - value_w
    bar_w = bar_x1 - bar_x0

    y = header_h + legend_h
    for idx, section in enumerate(OVERLAY_SECTIONS):
        fill = "#0f1620" if idx % 2 == 0 else "#11202d"
        rect((chart_x, y, chart_x + chart_w, y + section_h), fill)
        title = section["label"]
        draw.text(xy(chart_x + 16, y + 14), title, font=scenario_font, fill=hex_rgb("#f0f6fc"))
        scale_max, scale_source = cold_max_for_section(
            rows_by_key,
            benchmark,
            section["cold_key"],
            section["warm_key"],
        )
        speedup = overlay_speedup_label(rows_by_key, benchmark, section["warm_key"])
        if speedup:
            speedup_w = text_width(draw, speedup, subtitle_font) // scale
            draw.text(
                xy(chart_x + chart_w - 16 - speedup_w, y + 20),
                speedup,
                font=subtitle_font,
                fill=hex_rgb("#f85149"),
            )

        if scale_source == "cold":
            scale_note = f"100% = cold max {seconds_label(scale_max)}"
        elif scale_source == "warm":
            scale_note = f"100% = warm max {seconds_label(scale_max)} (no cold data)"
        else:
            scale_note = "no data"
        draw.text(xy(chart_x + 16, y + 42), scale_note, font=small_font, fill=hex_rgb("#8b949e"))

        row_y = y + 76
        for tool in TOOL_ORDER:
            cold_value = rows_by_key.get((benchmark, section["cold_key"], tool), {}).get("wall_ms")
            warm_value = rows_by_key.get((benchmark, section["warm_key"], tool), {}).get("wall_ms")
            label = tool_labels.get(tool, tool)
            colors = TOOL_COLOR_PAIRS[tool]
            warm_violation = (
                isinstance(cold_value, (int, float))
                and isinstance(warm_value, (int, float))
                and cold_value > 0
                and warm_value > cold_value * 1.10
            )

            draw.text(xy(chart_x + 16, row_y + 14), label, font=tool_font, fill=hex_rgb(colors["warm"]))
            track_y0 = row_y + 20
            rect((bar_x0, track_y0 + 10, bar_x1, track_y0 + 14), "#21262d")
            if isinstance(cold_value, (int, float)) and cold_value > 0:
                width_fraction = max(0.01, min(1.0, float(cold_value) / scale_max))
                bar_end = bar_x0 + max(3, int(bar_w * width_fraction))
                rect((bar_x0, track_y0, bar_end, track_y0 + 24), colors["cold"])
            if isinstance(warm_value, (int, float)) and warm_value > 0:
                width_fraction = max(0.01, min(1.0, float(warm_value) / scale_max))
                bar_end = bar_x0 + max(3, int(bar_w * width_fraction))
                rect(
                    (bar_x0, track_y0 + 5, bar_end, track_y0 + 19),
                    colors["warm"],
                    outline=WARM_VIOLATION_OUTLINE if warm_violation else None,
                    width_px=2 if warm_violation else 1,
                )
            draw.text(
                xy(bar_x1 + 12, row_y + 2),
                f"cold {seconds_label(cold_value)}",
                font=value_font,
                fill=hex_rgb(colors["cold"] if isinstance(cold_value, (int, float)) and cold_value > 0 else "#6e7681"),
            )
            draw.text(
                xy(bar_x1 + 12, row_y + 22),
                f"warm {seconds_label(warm_value)}",
                font=value_font,
                fill=hex_rgb(
                    WARM_VIOLATION_OUTLINE
                    if warm_violation
                    else colors["warm"]
                    if isinstance(warm_value, (int, float)) and warm_value > 0
                    else "#6e7681"
                ),
            )
            cache_value = rows_by_key.get((benchmark, section["warm_key"], tool), {}).get("cache_bytes")
            draw.text(
                xy(bar_x1 + 12, row_y + 42),
                f"cache {bytes_label(cache_value)}",
                font=small_font,
                fill=hex_rgb("#8b949e"),
            )
            row_y += 54

        draw.line(
            (xy(chart_x + 16, y + section_h - 1), xy(chart_x + chart_w - 16, y + section_h - 1)),
            fill=hex_rgb("#30363d"),
            width=scale,
        )
        y += section_h + section_gap

    footer = "Artifacts: latest.json, benchmark-rust-only.jpg, benchmark-rust-c.jpg"
    draw.line((xy(margin, height - 34), xy(width - margin, height - 34)), fill=hex_rgb("#30363d"), width=scale)
    draw.text(xy(margin, height - 25), footer, font=small_font, fill=hex_rgb("#8b949e"))

    resampling = getattr(Image, "Resampling", Image).LANCZOS
    image = image.resize((width, height), resampling)
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    output = OUT_DIR / meta["output"]
    image.save(output, format="JPEG", quality=90, optimize=True)
    return output


def main() -> int:
    if not INPUT.exists():
        print(f"render: missing {INPUT}", file=sys.stderr)
        return 1
    doc = load_comparison()
    for benchmark in BENCHMARKS:
        output = render_benchmark(doc, benchmark)
        print(f"render: wrote {output}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
