"""Tests for the thin-v2 second-build no-op verifier.

Covers the parsing layer (``parse_build_log``) and the assertion layer
(``assert_second_build_is_noop``) plus a thin CLI smoke check that exercises
the script as a subprocess, the same way CI invokes it.
"""

from __future__ import annotations

import importlib.util
import subprocess
import sys
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = REPO_ROOT / ".github" / "scripts" / "assert_thin_noop.py"


def _load_module():
    spec = importlib.util.spec_from_file_location("assert_thin_noop", SCRIPT_PATH)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    # Register before exec so dataclasses can resolve forward refs / module dict.
    sys.modules["assert_thin_noop"] = module
    spec.loader.exec_module(module)
    return module


@pytest.fixture(scope="module")
def mod():
    return _load_module()


# ---------------------------------------------------------------------------
# parse_build_log
# ---------------------------------------------------------------------------


def test_parse_build_log_extracts_workspace_path_dep(mod) -> None:
    text = (
        "   Compiling soldr-core v0.7.11 (/home/runner/work/soldr/crates/soldr-core)\n"
        "   Compiling serde v1.0.219\n"
        "    Finished `dev` profile [unoptimized + debuginfo] target(s) in 12.4s\n"
    )
    summary = mod.parse_build_log(text)
    assert summary.finished_seen is True
    assert len(summary.compiling_units) == 2
    fp = summary.first_party_compiles
    tp = summary.third_party_compiles
    assert len(fp) == 1
    assert fp[0].name == "soldr-core"
    assert fp[0].version == "0.7.11"
    assert fp[0].path is not None
    assert len(tp) == 1
    assert tp[0].name == "serde"


def test_parse_build_log_recognizes_finished_only(mod) -> None:
    text = "    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.12s\n"
    summary = mod.parse_build_log(text)
    assert summary.finished_seen is True
    assert summary.compiling_units == []
    assert summary.first_party_compiles == []
    assert summary.third_party_compiles == []


def test_parse_build_log_counts_fresh_lines(mod) -> None:
    text = (
        "       Fresh serde v1.0.219\n"
        "       Fresh soldr-core v0.7.11\n"
        "    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.05s\n"
    )
    summary = mod.parse_build_log(text)
    assert summary.fresh_count == 2
    assert summary.compiling_units == []


def test_parse_build_log_handles_windows_paths(mod) -> None:
    text = (
        r"   Compiling soldr-cli v0.7.11 (D:\a\soldr\soldr\crates\soldr-cli)" + "\n"
        "    Finished `dev` profile in 1.0s\n"
    )
    summary = mod.parse_build_log(text)
    assert len(summary.first_party_compiles) == 1
    assert summary.first_party_compiles[0].name == "soldr-cli"


# ---------------------------------------------------------------------------
# assert_second_build_is_noop
# ---------------------------------------------------------------------------


def _cold_log() -> str:
    return (
        "   Compiling proc-macro2 v1.0.86\n"
        "   Compiling serde v1.0.219\n"
        "   Compiling soldr-core v0.7.11 (/repo/crates/soldr-core)\n"
        "   Compiling soldr-cli v0.7.11 (/repo/crates/soldr-cli)\n"
        "    Finished `dev` profile in 42.1s\n"
    )


def _warm_noop_log() -> str:
    return "    Finished `dev` profile in 0.04s\n"


def test_assert_second_build_is_noop_passes_on_clean_warm(mod) -> None:
    _, _, errors = mod.assert_second_build_is_noop(_cold_log(), _warm_noop_log())
    assert errors == []


def test_assert_second_build_is_noop_allows_small_third_party_drift(mod) -> None:
    warm = (
        "   Compiling proc-macro-hack v0.5.20\n"
        "   Compiling pin-project-internal v1.1.5\n"
        "    Finished `dev` profile in 1.4s\n"
    )
    _, _, errors = mod.assert_second_build_is_noop(_cold_log(), warm, tolerance=2)
    assert errors == []


def test_assert_second_build_is_noop_fails_on_first_party_compile(mod) -> None:
    warm = (
        "   Compiling soldr-cli v0.7.11 (/repo/crates/soldr-cli)\n"
        "    Finished `dev` profile in 6.2s\n"
    )
    _, _, errors = mod.assert_second_build_is_noop(_cold_log(), warm)
    assert errors, "expected first-party recompile to fail the gate"
    assert any("first-party" in e for e in errors)


def test_assert_second_build_is_noop_fails_when_third_party_exceeds_tolerance(
    mod,
) -> None:
    warm_lines = [f"   Compiling crate{i} v0.{i}.0\n" for i in range(5)]
    warm = "".join(warm_lines) + "    Finished `dev` profile in 9.0s\n"
    _, _, errors = mod.assert_second_build_is_noop(_cold_log(), warm, tolerance=2)
    assert errors
    assert any("third-party" in e and "tolerance is 2" in e for e in errors)


def test_assert_second_build_is_noop_fails_when_warm_has_no_finished(mod) -> None:
    warm = "   Compiling something v0.1.0\n"  # truncated; no Finished line
    _, _, errors = mod.assert_second_build_is_noop(_cold_log(), warm)
    assert any("Finished" in e for e in errors)


def test_assert_second_build_is_noop_fails_on_empty_second_by_default(mod) -> None:
    _, _, errors = mod.assert_second_build_is_noop(_cold_log(), "")
    assert any("Finished" in e for e in errors)


def test_assert_second_build_is_noop_allows_empty_second_when_enabled(mod) -> None:
    _, _, errors = mod.assert_second_build_is_noop(
        _cold_log(),
        "",
        allow_empty_second=True,
    )
    assert errors == []


def test_assert_second_build_is_noop_fails_on_empty_first_by_default(mod) -> None:
    _, _, errors = mod.assert_second_build_is_noop(
        "    Finished `dev` profile in 0.01s\n",
        _warm_noop_log(),
    )
    assert any("first build did not show any Compiling lines" in e for e in errors)


def test_assert_second_build_is_noop_allow_empty_first_skips_baseline_check(
    mod,
) -> None:
    _, _, errors = mod.assert_second_build_is_noop(
        "    Finished `dev` profile in 0.01s\n",
        _warm_noop_log(),
        require_first_built_something=False,
    )
    assert errors == []


def test_assert_incomplete_restore_rebuilds_workspace_unit(mod) -> None:
    restored = (
        "       Fresh serde v1.0.219\n"
        "   Compiling verify-noop v0.1.0 (/tmp/verify-noop)\n"
        "    Finished `dev` profile in 0.4s\n"
    )
    _, second, errors = mod.assert_incomplete_restore_rebuilds(_cold_log(), restored)
    assert errors == []
    assert [unit.name for unit in second.first_party_compiles] == ["verify-noop"]


def test_assert_incomplete_restore_rejects_false_workspace_fresh(mod) -> None:
    restored = (
        "       Fresh verify-noop v0.1.0 (/tmp/verify-noop)\n"
        "    Finished `dev` profile in 0.04s\n"
    )
    _, _, errors = mod.assert_incomplete_restore_rebuilds(_cold_log(), restored)
    assert any("did not rebuild a first-party unit" in error for error in errors)


# ---------------------------------------------------------------------------
# CLI surface
# ---------------------------------------------------------------------------


def _write(path: Path, body: str) -> Path:
    path.write_text(body, encoding="utf-8")
    return path


def test_cli_exits_zero_on_clean_warm_build(tmp_path: Path) -> None:
    first = _write(tmp_path / "first.log", _cold_log())
    second = _write(tmp_path / "second.log", _warm_noop_log())
    result = subprocess.run(
        [sys.executable, str(SCRIPT_PATH), str(first), str(second)],
        capture_output=True,
        text=True,
    )
    assert result.returncode == 0, result.stderr
    assert "assert_thin_noop: OK" in result.stdout


def test_cli_allows_empty_second_with_flag(tmp_path: Path) -> None:
    first = _write(tmp_path / "first.log", _cold_log())
    second = _write(tmp_path / "second.log", "")
    result = subprocess.run(
        [
            sys.executable,
            str(SCRIPT_PATH),
            str(first),
            str(second),
            "--allow-empty-second",
        ],
        capture_output=True,
        text=True,
    )
    assert result.returncode == 0, result.stderr
    assert "assert_thin_noop: OK" in result.stdout


def test_cli_exits_one_on_first_party_recompile(tmp_path: Path) -> None:
    first = _write(tmp_path / "first.log", _cold_log())
    second = _write(
        tmp_path / "second.log",
        (
            "   Compiling soldr-cli v0.7.11 (/repo/crates/soldr-cli)\n"
            "    Finished `dev` profile in 6.2s\n"
        ),
    )
    result = subprocess.run(
        [sys.executable, str(SCRIPT_PATH), str(first), str(second)],
        capture_output=True,
        text=True,
    )
    assert result.returncode == 1
    assert "FAIL" in result.stderr
    assert "first-party" in result.stderr


def test_cli_exits_two_on_missing_log(tmp_path: Path) -> None:
    missing = tmp_path / "does_not_exist.log"
    second = _write(tmp_path / "second.log", _warm_noop_log())
    result = subprocess.run(
        [sys.executable, str(SCRIPT_PATH), str(missing), str(second)],
        capture_output=True,
        text=True,
    )
    assert result.returncode == 2
    assert "input log not found" in result.stderr


def test_cli_tolerance_flag_relaxes_third_party_gate(tmp_path: Path) -> None:
    first = _write(tmp_path / "first.log", _cold_log())
    warm_lines = [f"   Compiling crate{i} v0.{i}.0\n" for i in range(5)]
    second = _write(
        tmp_path / "second.log",
        "".join(warm_lines) + "    Finished `dev` profile in 9.0s\n",
    )
    # Default tolerance (2) should fail.
    fail = subprocess.run(
        [sys.executable, str(SCRIPT_PATH), str(first), str(second)],
        capture_output=True,
        text=True,
    )
    assert fail.returncode == 1
    # Bumping tolerance past the count should pass.
    ok = subprocess.run(
        [
            sys.executable,
            str(SCRIPT_PATH),
            str(first),
            str(second),
            "--tolerance",
            "10",
        ],
        capture_output=True,
        text=True,
    )
    assert ok.returncode == 0, ok.stderr


def test_cli_expect_incomplete_restore_requires_workspace_rebuild(tmp_path: Path) -> None:
    first = _write(tmp_path / "first.log", _cold_log())
    second = _write(
        tmp_path / "second.log",
        "   Compiling verify-noop v0.1.0 (/tmp/verify-noop)\n"
        "    Finished `dev` profile in 0.4s\n",
    )
    result = subprocess.run(
        [
            sys.executable,
            str(SCRIPT_PATH),
            str(first),
            str(second),
            "--expect-incomplete-restore",
        ],
        capture_output=True,
        text=True,
    )
    assert result.returncode == 0, result.stderr
    assert "correctly rebuilt missing primary outputs" in result.stdout


# --- soldr#1951: elapsed-second stamping (soldr#1915) ------------------------


# Verbatim shape from the failing lane, including the leading spaces cargo
# emits before "Compiling" and the stamp column soldr#1915 inserts.
_STAMPED_COLD_LOG = """\
    0.01    Updating crates.io index
   21.07    Compiling thiserror v1.0.69
   21.42    Compiling serde v1.0.229
   22.90     Finished `dev` profile [unoptimized] target(s) in 22.90s
"""

_UNSTAMPED_COLD_LOG = """\
    Updating crates.io index
    Compiling thiserror v1.0.69
    Compiling serde v1.0.229
     Finished `dev` profile [unoptimized] target(s) in 22.90s
"""


def test_detect_timestamp_prefix_spots_a_stamped_log(mod) -> None:
    assert mod.detect_timestamp_prefix(_STAMPED_COLD_LOG) is True


def test_detect_timestamp_prefix_ignores_a_normal_log(mod) -> None:
    assert mod.detect_timestamp_prefix(_UNSTAMPED_COLD_LOG) is False


def test_stamped_log_still_defeats_the_anchored_patterns(mod) -> None:
    """The bug itself. Kept as a test so the detector's premise stays true."""
    summary = mod.parse_build_log(_STAMPED_COLD_LOG)
    assert summary.compiling_units == []
    # `Finished` is the only unanchored pattern, which is exactly why it was
    # the one field that still came back true in the CI failure.
    assert summary.finished_seen is True


def test_a_stamped_log_reports_the_stamp_not_an_empty_build(mod) -> None:
    _first, _second, errors = mod.assert_incomplete_restore_rebuilds(
        _STAMPED_COLD_LOG, _STAMPED_COLD_LOG
    )
    joined = " ".join(errors)
    assert "SOLDR_TIMESTAMP_LINES=0" in joined, joined
    assert "soldr#1951" in joined, joined
    # The misleading claim must be gone: a stamped log is not evidence that
    # nothing compiled.
    assert "did not show any Compiling lines" not in joined, joined


def test_an_unstamped_empty_build_still_reports_the_plain_message(mod) -> None:
    empty = "     Finished `dev` profile [unoptimized] target(s) in 0.01s\n"
    _first, _second, errors = mod.assert_incomplete_restore_rebuilds(empty, empty)
    joined = " ".join(errors)
    assert "did not show any Compiling lines" in joined, joined
    assert "SOLDR_TIMESTAMP_LINES" not in joined, joined
