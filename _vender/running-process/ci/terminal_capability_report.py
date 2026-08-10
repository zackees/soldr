"""Merge terminal graphics capability JSON exports and write a CI summary."""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print("usage: python -m ci.terminal_capability_report <export-dir>", file=sys.stderr)
        return 2

    export_dir = Path(argv[1])
    files = sorted(
        path
        for path in export_dir.glob("*.json")
        if path.name != "capability-aggregate.json"
    )
    if not files:
        print(f"no terminal capability JSON files found in {export_dir}", file=sys.stderr)
        return 1

    records = []
    for path in files:
        data = json.loads(path.read_text(encoding="utf-8"))
        records.append({"file": path.name, "data": data})

    aggregate = {
        "export_dir": str(export_dir),
        "case_count": len(records),
        "records": records,
    }
    aggregate_path = export_dir / "capability-aggregate.json"
    aggregate_path.write_text(json.dumps(aggregate, indent=2), encoding="utf-8")

    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with Path(summary).open("a", encoding="utf-8") as handle:
            handle.write("## Terminal Graphics Capability Matrix\n\n")
            handle.write(f"- Export directory: `{export_dir}`\n")
            handle.write(f"- JSON files: `{len(records)}`\n")
            handle.write(f"- Aggregate: `{aggregate_path}`\n\n")
            handle.write("| Case | Sixel | Evidence | Preferred | Source |\n")
            handle.write("| --- | --- | --- | --- | --- |\n")
            for record in records:
                data = record["data"]
                case = record["file"].removesuffix(".json")
                if isinstance(data, list):
                    continue
                graphics = data.get("graphics", {})
                protocols = graphics.get("protocols", [])
                sixel = next(
                    (p for p in protocols if p.get("protocol") == "sixel"),
                    {},
                )
                handle.write(
                    "| {case} | {status} | {evidence} | {preferred} | {source} |\n".format(
                        case=case,
                        status=sixel.get("status", ""),
                        evidence=sixel.get("evidence", ""),
                        preferred=graphics.get("preferred") or "",
                        source=sixel.get("source", ""),
                    )
                )

    print(f"wrote {aggregate_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
