"""Offline tests for `ci/reld_live_fetch_probe.py` (soldr#3359)."""

from __future__ import annotations

import json
from pathlib import Path

from conftest import load_script_module

probe = load_script_module(
    Path(__file__).resolve().parents[1] / "ci" / "reld_live_fetch_probe.py"
)


def _artifact(name: str, executable: str | None) -> str:
    return json.dumps(
        {
            "reason": "compiler-artifact",
            "target": {"name": name, "kind": ["bin"]},
            "executable": executable,
        }
    )


def test_linked_executable_follows_cargo_into_the_target_triple_dir() -> None:
    # soldr passes `--target <host>` on Windows, so Cargo writes the binary
    # under `target/<triple>/debug`, not `target/debug` (the soldr#3359
    # Windows failure: the probe guessed the wrong directory).
    exe = r"C:\p\target\x86_64-pc-windows-msvc\debug\reld-live-probe.exe"
    stdout = "\n".join(
        [
            "not json",
            _artifact("build-script-build", None),
            _artifact("reld-live-probe", exe),
            json.dumps({"reason": "build-finished", "success": True}),
        ]
    )
    assert probe.linked_executable(stdout) == Path(exe)


def test_linked_executable_is_none_without_an_executable_artifact() -> None:
    assert probe.linked_executable(_artifact("reld-live-probe", None)) is None
    assert probe.linked_executable("") is None


def test_reld_linked_outputs_reads_successful_records_on_any_separator(
    tmp_path: Path,
) -> None:
    log = tmp_path / "inv.jsonl"
    records = [
        {"status": "success", "output": r"C:\stage\.compile-1\reld_live_probe.exe"},
        {"status": "success", "output": "/tmp/stage/reld_live_probe-0123abcd"},
        {"status": "failure", "output": "/tmp/stage/other"},
        {"status": "success"},
    ]
    log.write_text("\n".join(json.dumps(r) for r in records), encoding="utf-8")
    assert probe.reld_linked_outputs(log) == [
        "reld_live_probe.exe",
        "reld_live_probe-0123abcd",
    ]
    assert probe.reld_linked_outputs(tmp_path / "missing.jsonl") == []
