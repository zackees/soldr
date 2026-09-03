#!/usr/bin/env python3
from __future__ import annotations

import json
import statistics
import sys
from pathlib import Path


def is_external(package_id: str) -> bool:
    return package_id.startswith(("registry+", "git+"))


def main() -> int:
    messages_path = Path(sys.argv[1])
    stderr_path = Path(sys.argv[2])
    seed_ms = int(Path(sys.argv[3]).read_text(encoding="utf-8").strip())
    warm_ms = int(Path(sys.argv[4]).read_text(encoding="utf-8").strip())
    warm_samples = [
        int(line)
        for line in Path(sys.argv[5]).read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]
    median_warm_ms = int(statistics.median(warm_samples))
    external_dirty: list[str] = []
    external_build_script_messages: list[str] = []
    external_fresh: set[str] = set()
    workspace_dirty: set[str] = set()

    for raw in messages_path.read_text(encoding="utf-8").splitlines():
        try:
            message = json.loads(raw)
        except json.JSONDecodeError:
            continue
        package_id = str(message.get("package_id", ""))
        reason = message.get("reason")
        if reason == "compiler-artifact":
            if message.get("fresh") is True:
                if is_external(package_id):
                    external_fresh.add(package_id)
            elif is_external(package_id):
                external_dirty.append(package_id)
            elif package_id:
                workspace_dirty.add(package_id)
        elif reason == "build-script-executed" and is_external(package_id):
            # Cargo emits this message when it reuses cached build-script output,
            # too. Actual process execution is identified from Cargo -vv below.
            external_build_script_messages.append(package_id)

    external_build_script_runs = [
        line
        for line in stderr_path.read_text(encoding="utf-8").splitlines()
        if " Running " in line
        and "build-script-build" in line
        and ("/root/.cargo/registry/" in line or "/root/.cargo/git/" in line)
    ]

    report = {
        "schema_version": 1,
        "seed_ms": seed_ms,
        "warm_ms": warm_ms,
        "warm_samples_ms": warm_samples,
        "median_warm_ms": median_warm_ms,
        "speedup": round(seed_ms / max(warm_ms, 1), 3),
        "median_speedup": round(seed_ms / max(median_warm_ms, 1), 3),
        "external_fresh": len(external_fresh),
        "external_dirty": sorted(set(external_dirty)),
        "external_build_script_messages": sorted(set(external_build_script_messages)),
        "external_build_script_runs": external_build_script_runs,
        "workspace_dirty": sorted(workspace_dirty),
    }
    print(json.dumps(report, indent=2, sort_keys=True))
    report_path = (
        Path(sys.argv[6]) if len(sys.argv) > 6 else Path("/artifact/warm-report.json")
    )
    report_path.parent.mkdir(parents=True, exist_ok=True)
    report_path.write_text(
        json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )

    if report["external_dirty"]:
        raise SystemExit(
            f"external dependencies recompiled: {report['external_dirty']}"
        )
    if report["external_build_script_runs"]:
        raise SystemExit(
            f"external build scripts reran: {report['external_build_script_runs']}"
        )
    if not external_fresh:
        raise SystemExit("Cargo reported no fresh external dependency artifacts")
    if not workspace_dirty:
        raise SystemExit(
            "expected the real workspace package to compile after hydration"
        )
    if warm_ms * 10 > seed_ms:
        raise SystemExit(
            f"warm dependency build missed 10x gate: seed={seed_ms}ms warm={warm_ms}ms"
        )
    if len(warm_samples) >= 3 and median_warm_ms * 10 > seed_ms:
        raise SystemExit(
            "median warm dependency build missed 10x gate: "
            f"seed={seed_ms}ms median_warm={median_warm_ms}ms samples={warm_samples}"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
