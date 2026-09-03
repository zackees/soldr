"""Unit coverage for ci/macos_recovery_run.py (soldr#3076).

The per-PR `e2e-macos-x64` lane in `ci.yml` (via `_ci-target-run.yml`)
executes this script's guest script inside a zackees/docker-mac-x64
Recovery guest and verifies the collected results with it. The guest never
runs under test here -- these pin the script's text and the collected-result
parsing, the two pure surfaces.
"""

from pathlib import Path

from conftest import (
    assert_recovery_verify_collected_contract,
    load_script_module,
    write_collected_recovery_summary,
)

REPO_ROOT = Path(__file__).resolve().parents[1]
MODULE = load_script_module(
    REPO_ROOT / "ci" / "macos_recovery_run.py", "macos_recovery_run"
)


def test_build_guest_script_is_bash_3_2_compatible_and_self_contained() -> None:
    script = MODULE.build_guest_script()
    assert script.startswith("#!/bin/sh")
    assert f"curl -fsS -o /tmp/soldr {MODULE.GUEST_HTTP_BASE}/soldr" in script
    assert "/tmp/soldr --version" in script
    assert "/tmp/soldr --help" in script
    assert 'exit "$FAIL"' in script


def test_build_guest_script_writes_every_declared_check() -> None:
    script = MODULE.build_guest_script()
    for name in MODULE.CHECKS:
        assert f"{name}=pass" in script or f'"{name}=' in script


def test_parse_summary_splits_status_and_detail() -> None:
    text = "arch=pass:x86_64\nversion=fail:not soldr\n"
    results = MODULE.parse_summary(text)
    assert results["arch"] == (True, "x86_64")
    assert results["version"] == (False, "not soldr")


def test_parse_summary_flags_malformed_lines() -> None:
    results = MODULE.parse_summary("garbage line with no equals\n")
    assert results["summary_line_1"][0] is False


def _passing_summary_lines() -> list[str]:
    return [f"{name}=pass:ok" for name in MODULE.CHECKS]


def test_verify_collected_matches_the_shared_recovery_contract(tmp_path: Path) -> None:
    assert_recovery_verify_collected_contract(
        MODULE, tmp_path, passing_lines=_passing_summary_lines()
    )


def test_main_emit_guest_script_writes_the_output_file(tmp_path: Path) -> None:
    output = tmp_path / "recovery-run.sh"
    rc = MODULE.main(["emit-guest-script", "--output", str(output)])
    assert rc == 0
    assert output.read_text(encoding="utf-8").startswith("#!/bin/sh")


def test_main_verify_collected_delegates_to_verify_collected(tmp_path: Path) -> None:
    collected = write_collected_recovery_summary(
        tmp_path / "collected", _passing_summary_lines()
    )
    rc = MODULE.main(
        [
            "verify-collected",
            "--collected",
            str(collected),
            "--guest-exit-code",
            "0",
        ]
    )
    assert rc == 0
