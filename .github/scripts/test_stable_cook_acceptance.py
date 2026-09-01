"""Tests for stable_cook_acceptance.py (soldr#3043 Phase 2).

No Docker, no subprocess: these exercise only the pure Python pieces that
run host-side after the BASH scenario matrix has already produced its JSON
rows -- row parsing, the expected-outcome table, and the summary renderer.
"""

from __future__ import annotations

from pathlib import Path

from _script_loader import load_script_module

SCRIPT = Path(__file__).with_name("stable_cook_acceptance.py")
stable_cook_acceptance = load_script_module(SCRIPT, "stable_cook_acceptance")


# --- EXPECTED_OUTCOMES -------------------------------------------------------


def test_expected_outcomes_has_exactly_the_three_modes_in_order() -> None:
    assert list(stable_cook_acceptance.EXPECTED_OUTCOMES) == [
        "cold",
        "warm",
        "object_cache_only",
    ]


def test_cold_and_object_cache_only_only_accept_built() -> None:
    assert stable_cook_acceptance.EXPECTED_OUTCOMES["cold"] == frozenset({"built"})
    assert stable_cook_acceptance.EXPECTED_OUTCOMES["object_cache_only"] == frozenset(
        {"built"}
    )


# --- parse_rows ---------------------------------------------------------------


def test_parse_rows_skips_non_json_lines() -> None:
    lines = [
        "host triple: x86_64-unknown-linux-gnu\n",
        "Compiling soldr-core v0.1.0\n",
        '{"name": "cold", "outcome": "built", "wall_ms": 1000}\n',
        "not json at all {{{\n",
    ]
    rows = stable_cook_acceptance.parse_rows(lines)
    assert rows == [{"name": "cold", "outcome": "built", "wall_ms": 1000}]


def test_parse_rows_keeps_only_dicts_with_name_and_outcome() -> None:
    lines = [
        '{"name": "cold", "wall_ms": 1000}\n',  # no outcome
        '{"outcome": "built", "wall_ms": 1000}\n',  # no name
        '["name", "cold"]\n',  # not an object
        "42\n",  # not an object
        '{"name": "warm", "outcome": "hydrated"}\n',
    ]
    rows = stable_cook_acceptance.parse_rows(lines)
    assert rows == [{"name": "warm", "outcome": "hydrated"}]


def test_parse_rows_preserves_scenario_order() -> None:
    lines = [
        '{"name": "cold", "outcome": "built"}\n',
        '{"name": "warm", "outcome": "hydrated"}\n',
        '{"name": "object_cache_only", "outcome": "built"}\n',
    ]
    rows = stable_cook_acceptance.parse_rows(lines)
    assert [row["name"] for row in rows] == ["cold", "warm", "object_cache_only"]


# --- outcome_accepted -----------------------------------------------------------


def test_warm_accepts_hydrated() -> None:
    assert stable_cook_acceptance.outcome_accepted("warm", "hydrated") is True


def test_warm_accepts_warm_skip() -> None:
    assert stable_cook_acceptance.outcome_accepted("warm", "warm-skip") is True


def test_warm_rejects_built() -> None:
    assert stable_cook_acceptance.outcome_accepted("warm", "built") is False


def test_cold_rejects_hydrated() -> None:
    assert stable_cook_acceptance.outcome_accepted("cold", "hydrated") is False


def test_object_cache_only_accepts_built_but_not_hydrated() -> None:
    assert stable_cook_acceptance.outcome_accepted("object_cache_only", "built") is True
    assert (
        stable_cook_acceptance.outcome_accepted("object_cache_only", "hydrated")
        is False
    )


def test_unknown_mode_accepts_nothing() -> None:
    assert stable_cook_acceptance.outcome_accepted("not-a-mode", "built") is False


# --- render_summary -------------------------------------------------------------


def test_render_summary_produces_one_table_row_per_scenario() -> None:
    rows = [
        {
            "name": "cold",
            "outcome": "built",
            "detail": "",
            "wall_ms": 120000,
            "exit_code": 0,
            "archive_bytes": 2 * 1024 * 1024,
        },
        {
            "name": "warm",
            "outcome": "hydrated",
            "detail": "",
            "wall_ms": 3000,
            "exit_code": 0,
            "archive_bytes": 2 * 1024 * 1024,
        },
        {
            "name": "object_cache_only",
            "outcome": "built",
            "detail": "",
            "wall_ms": 90000,
            "exit_code": 0,
            "archive_bytes": 2 * 1024 * 1024,
        },
    ]
    summary = stable_cook_acceptance.render_summary(rows)
    table_rows = [
        line
        for line in summary.splitlines()
        if line.startswith("|") and "---" not in line and "Scenario" not in line
    ]
    assert len(table_rows) == len(rows)
    for row in rows:
        assert str(row["name"]) in summary
        assert str(row["outcome"]) in summary


def test_render_summary_reports_archive_size_in_mib() -> None:
    rows = [
        {
            "name": "cold",
            "outcome": "built",
            "detail": "",
            "wall_ms": 1,
            "exit_code": 0,
            "archive_bytes": 2 * 1024 * 1024,
        }
    ]
    summary = stable_cook_acceptance.render_summary(rows)
    assert "2.0" in summary
    assert "soldr#3047" in summary


def test_render_summary_with_no_rows_still_has_a_header() -> None:
    summary = stable_cook_acceptance.render_summary([])
    assert "Scenario" in summary
