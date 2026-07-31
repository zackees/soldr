"""A repeated build must recompile nothing (soldr#1799).

The failure this guards is a fingerprint invalidation — a toolchain-home flip
or compiler-path change — which makes cargo treat finished work as stale so
every warm build recompiles the world. Nothing errors; it is just slow. Running
the same build twice back to back is the cheapest probe: nothing changed, so
the second pass must compile nothing.
"""

from __future__ import annotations

from pathlib import Path

import pytest
from conftest import load_script_module

SCRIPT = (
    Path(__file__).resolve().parents[1]
    / ".github"
    / "scripts"
    / "check_warm_rebuild.py"
)


@pytest.fixture(scope="module")
def guard():
    return load_script_module(SCRIPT, "check_warm_rebuild")


def test_a_truly_warm_build_reports_nothing(guard):
    output = "    0.92     Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.85s\n"
    assert guard.recompiled_crates(output) == []


def test_a_recompiled_crate_is_detected(guard):
    output = "   Compiling soldr-core v0.8.29 (/work/crates/soldr-core)\n   Finished\n"
    assert guard.recompiled_crates(output) == ["soldr-core"]


def test_soldr_elapsed_prefixes_are_tolerated(guard):
    # soldr#1802 stamps each relayed line with elapsed seconds, so the build
    # output this reads is normally prefixed. A parser anchored at column 0
    # would silently see nothing and always pass.
    output = "   12.34     Compiling soldr-daemon v0.8.29 (/work/crates/soldr-daemon)\n"
    assert guard.recompiled_crates(output) == ["soldr-daemon"]


def test_ansi_colour_is_tolerated(guard):
    output = "\x1b[0m\x1b[1m\x1b[32m   Compiling\x1b[0m serde v1.0.0\n"
    assert guard.recompiled_crates(output) == ["serde"]


def test_fresh_lines_are_not_recompilation(guard):
    # `Fresh` is cargo's verbose way of saying it did nothing; counting it
    # would make every warm build look like a failure.
    output = "       Fresh serde v1.0.0\n       Fresh soldr-core v0.8.29\n"
    assert guard.recompiled_crates(output) == []


def test_repeated_crates_are_reported_once(guard):
    output = (
        "   Compiling serde v1.0.0\n"
        "   Compiling serde v1.0.0\n"
        "   Compiling soldr-core v0.8.29\n"
    )
    assert guard.recompiled_crates(output) == ["serde", "soldr-core"]


def test_main_fails_and_names_the_crates(guard, tmp_path, capsys):
    log = tmp_path / "warm.log"
    log.write_text("   Compiling soldr-core v0.8.29\n", encoding="utf-8")
    assert guard.main([str(log)]) == 1
    out = capsys.readouterr().out
    assert "should have been a no-op" in out
    assert "soldr-core" in out


def test_main_passes_on_a_warm_log(guard, tmp_path, capsys):
    log = tmp_path / "warm.log"
    log.write_text("    Finished `dev` profile in 0.85s\n", encoding="utf-8")
    assert guard.main([str(log)]) == 0
    assert "OK" in capsys.readouterr().out


def test_allow_threshold_tolerates_known_churn(guard, tmp_path):
    log = tmp_path / "warm.log"
    log.write_text("   Compiling a v1\n   Compiling b v1\n", encoding="utf-8")
    assert guard.main([str(log), "--allow", "2"]) == 0
    assert guard.main([str(log), "--allow", "1"]) == 1


def test_a_missing_log_does_not_fail_the_build(guard, tmp_path, capsys):
    # Plumbing gaps are not build failures.
    assert guard.main([str(tmp_path / "nope.log")]) == 0
    assert "could not read" in capsys.readouterr().out


def test_cargo_check_uses_a_different_verb_and_must_still_count(guard):
    # `cargo build` says "Compiling"; `cargo check` says "Checking". Matching
    # only one silently passes for the other, which is worse than no guard --
    # this was a real bug caught by running the guard against actual output.
    output = "    1.76     Checking soldr-core v0.8.29 (/work/crates/soldr-core)\n"
    assert guard.recompiled_crates(output) == ["soldr-core"]
