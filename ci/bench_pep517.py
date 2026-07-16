#!/usr/bin/env python3
"""Benchmark local PEP 517 editable and wheel installs.

The harness uses temporary virtual environments and temporary project copies,
but deliberately inherits the caller's Cargo, rustup, soldr, and zccache state.
It never deletes or redirects those caches. Each sample records its complete
install log so packaging, Cargo, cache, and linker phases can be inspected
after the wall-clock summary.

Run with ``uv run --no-project --script ci/bench_pep517.py --project PATH``.
Use ``--repetitions 1`` for a quick smoke run; the default is three samples
per scenario so reported medians are meaningful.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any


PHASE_PATTERNS = {
    "packaging": re.compile(r"wheel|editable|dist-info|setuptools|maturin", re.I),
    "cargo": re.compile(r"cargo|compiling|fresh|finished|rustc", re.I),
    "cache": re.compile(r"zccache|cache hit|cache miss|cached|reuse", re.I),
    "linking": re.compile(r"linker|linking|rust-lld|lld|mold|link\.exe", re.I),
}


def _python_path(venv: Path) -> Path:
    return venv / ("Scripts/python.exe" if os.name == "nt" else "bin/python")


def _run(cmd: list[str], *, cwd: Path | None = None) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        cmd,
        cwd=str(cwd) if cwd else None,
        check=False,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        encoding="utf-8",
        errors="replace",
    )


def _phase_counts(output: str) -> dict[str, int]:
    return {
        phase: sum(1 for line in output.splitlines() if pattern.search(line))
        for phase, pattern in PHASE_PATTERNS.items()
    }


def _copy_project(source: Path, destination: Path) -> None:
    ignored = shutil.ignore_patterns(
        ".git",
        ".venv",
        ".claude",
        "target",
        ".zccache",
        "__pycache__",
    )
    shutil.copytree(source, destination, ignore=ignored)


def _source_change_file(project: Path, requested: str | None) -> Path | None:
    if requested:
        candidate = project / requested
        return candidate if candidate.is_file() else None
    candidates = sorted(project.glob("crates/**/*.rs"))
    return candidates[0] if candidates else None


def _sample(
    *,
    scenario: str,
    repetition: int,
    project: Path,
    venv: Path,
    output_dir: Path,
    reinstall: bool,
    editable: bool,
) -> dict[str, Any]:
    install_args = [
        "uv",
        "pip",
        "install",
        "--python",
        str(_python_path(venv)),
        "--no-deps",
    ]
    if reinstall:
        install_args.append("--reinstall")
    if editable:
        install_args.append("-e")
    install_args.append(str(project))

    started = time.perf_counter()
    result = _run(install_args, cwd=project)
    elapsed = time.perf_counter() - started
    log_path = output_dir / "logs" / f"{scenario}-r{repetition}.log"
    log_path.parent.mkdir(parents=True, exist_ok=True)
    log_path.write_text(result.stdout, encoding="utf-8")
    return {
        "scenario": scenario,
        "repetition": repetition,
        "elapsed_s": round(elapsed, 3),
        "returncode": result.returncode,
        "command": install_args,
        "log": str(log_path),
        "phase_lines": _phase_counts(result.stdout),
    }


def _prepare_venv(root: Path, name: str) -> tuple[Path, Path]:
    venv = root / f"venv-{name}"
    result = _run(["uv", "venv", str(venv)])
    if result.returncode:
        raise RuntimeError(result.stdout)
    python = _python_path(venv)
    if not python.is_file():
        raise RuntimeError(f"uv did not create the expected interpreter: {python}")
    return venv, python


def _metadata(project: Path) -> dict[str, Any]:
    versions: dict[str, str] = {}
    for name, command in (("uv", ["uv", "--version"]), ("soldr", ["soldr", "--version"])):
        result = _run(command)
        versions[name] = result.stdout.strip()
    return {
        "platform": sys.platform,
        "python": sys.version.split()[0],
        "project": str(project),
        "versions": versions,
        "caches_preserved": True,
        "cache_environment": {
            name: os.environ.get(name)
            for name in (
                "CARGO_HOME",
                "RUSTUP_HOME",
                "CARGO_TARGET_DIR",
                "RUSTC_WRAPPER",
                "ZCCACHE_CACHE_DIR",
                "ZCCACHE_PATH_REMAP",
            )
            if os.environ.get(name)
        },
    }


def _median(samples: list[dict[str, Any]]) -> float | None:
    successful = [
        float(sample["elapsed_s"])
        for sample in samples
        if sample["returncode"] == 0
    ]
    return round(statistics.median(successful), 3) if successful else None


def benchmark(project: Path, output_dir: Path, repetitions: int, source_file: str | None) -> dict[str, Any]:
    results: dict[str, Any] = {"metadata": _metadata(project), "scenarios": {}}
    with tempfile.TemporaryDirectory(prefix="soldr-pep517-bench-") as raw:
        temp_root = Path(raw)
        for editable in (True, False):
            kind = "editable" if editable else "wheel"
            samples: dict[str, list[dict[str, Any]]] = {
                "cold": [],
                "warm_noop": [],
                "source_change": [],
            }
            for repetition in range(1, repetitions + 1):
                project_copy = temp_root / f"{kind}-project-{repetition}"
                _copy_project(project, project_copy)
                venv, _ = _prepare_venv(temp_root, f"{kind}-{repetition}")
                samples["cold"].append(
                    _sample(
                        scenario=f"{kind}-cold",
                        repetition=repetition,
                        project=project_copy,
                        venv=venv,
                        output_dir=output_dir,
                        reinstall=False,
                        editable=editable,
                    )
                )
                samples["warm_noop"].append(
                    _sample(
                        scenario=f"{kind}-warm-noop",
                        repetition=repetition,
                        project=project_copy,
                        venv=venv,
                        output_dir=output_dir,
                        reinstall=True,
                        editable=editable,
                    )
                )
                changed = _source_change_file(project_copy, source_file)
                if changed:
                    with changed.open("a", encoding="utf-8") as stream:
                        stream.write("\n")
                    samples["source_change"].append(
                        _sample(
                            scenario=f"{kind}-source-change",
                            repetition=repetition,
                            project=project_copy,
                            venv=venv,
                            output_dir=output_dir,
                            reinstall=True,
                            editable=editable,
                        )
                    )
            results["scenarios"][kind] = {
                name: {"median_s": _median(values), "samples": values}
                for name, values in samples.items()
            }
    return results


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--project", type=Path, required=True)
    parser.add_argument("--output", type=Path, default=Path("ci/bench-results/pep517.json"))
    parser.add_argument("--repetitions", type=int, default=3)
    parser.add_argument("--source-file", help="Rust source path relative to --project")
    args = parser.parse_args()
    project = args.project.resolve()
    if not project.is_dir():
        parser.error(f"project directory does not exist: {project}")
    if args.repetitions < 1:
        parser.error("--repetitions must be positive")
    output = args.output.resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    result = benchmark(project, output.parent, args.repetitions, args.source_file)
    output.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({name: {case: value["median_s"] for case, value in cases.items()} for name, cases in result["scenarios"].items()}, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
