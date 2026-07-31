"""The CI fallback guard must fail on a silent degrade and only then (soldr#1838).

Guards the pure `evaluate` core of `.github/scripts/check_compile_fallbacks.py`:
a recorded fallback fails; a clean or absent rollup passes; a malformed rollup
must never fail a build on the guard's own account.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from conftest import load_script_module

SCRIPT = (
    Path(__file__).resolve().parents[1]
    / ".github"
    / "scripts"
    / "check_compile_fallbacks.py"
)


@pytest.fixture(scope="module")
def guard():
    return load_script_module(SCRIPT, "check_compile_fallbacks")


def _doctor(total: int, reasons: "list[str] | None" = None) -> dict:
    recent = [{"ts_ms": 1000 + i, "reason": r} for i, r in enumerate(reasons or [])]
    return {"command": "doctor", "fallbacks": {"total": total, "recent": recent}}


def test_a_clean_rollup_passes(guard):
    ok, total, reasons = guard.evaluate(_doctor(0), allow=0)
    assert ok is True
    assert total == 0
    assert reasons == []


def test_any_fallback_fails_by_default(guard):
    ok, total, reasons = guard.evaluate(
        _doctor(2, ["daemon unavailable", "reply timed out"]), allow=0
    )
    assert ok is False
    assert total == 2
    assert reasons == ["daemon unavailable", "reply timed out"]


def test_allow_threshold_tolerates_up_to_n(guard):
    assert guard.evaluate(_doctor(3), allow=3)[0] is True
    assert guard.evaluate(_doctor(4), allow=3)[0] is False


def test_a_missing_fallbacks_block_is_treated_as_zero(guard):
    # The guard must not fail a build because the rollup was absent (an old
    # binary, a different command) — only because a fallback was recorded.
    ok, total, _ = guard.evaluate({"command": "doctor"}, allow=0)
    assert ok is True
    assert total == 0


def test_a_malformed_rollup_does_not_fail_the_build(guard):
    for bad in (
        {"fallbacks": None},
        {"fallbacks": {"total": "two"}},
        {"fallbacks": []},
    ):
        ok, total, _ = guard.evaluate(bad, allow=0)
        assert ok is True, f"malformed rollup {bad!r} must not fail the guard"
        assert total == 0


def test_main_exits_nonzero_on_fallbacks(guard, tmp_path, capsys):
    path = tmp_path / "doctor.json"
    path.write_text(json.dumps(_doctor(1, ["daemon unavailable: connection refused"])))
    code = guard.main([str(path)])
    assert code == 1
    out = capsys.readouterr().out
    assert "silently ran UNCACHED" in out
    assert "daemon unavailable: connection refused" in out


def test_main_exits_zero_on_clean(guard, tmp_path, capsys):
    path = tmp_path / "doctor.json"
    path.write_text(json.dumps(_doctor(0)))
    assert guard.main([str(path)]) == 0
    assert "OK" in capsys.readouterr().out


def test_main_passes_on_unreadable_input(guard, tmp_path, capsys):
    # A wiring problem (missing file, bad JSON) is not a build failure.
    missing = guard.main([str(tmp_path / "nope.json")])
    assert missing == 0
    bad = tmp_path / "bad.json"
    bad.write_text("{ not json")
    assert guard.main([str(bad)]) == 0
    assert "could not read" in capsys.readouterr().out
