"""The musl artifacts must really be statically linked (soldr#1060).

A musl target that silently picks up a dynamic link still builds and still
passes tests on the modern CI image — it only fails on the old distro the
artifact exists to serve. These tests pin the detection so the guard cannot
quietly become a no-op.
"""

from __future__ import annotations

import importlib.util
import sys
from pathlib import Path

import pytest

SCRIPT = (
    Path(__file__).resolve().parents[1]
    / ".github"
    / "scripts"
    / "verify_static_link.py"
)

# Real `readelf -d` output shapes.
STATIC_OUTPUT = "\nThere is no dynamic section in this file.\n"
DYNAMIC_OUTPUT = """
Dynamic section at offset 0x2dd8 contains 27 entries:
  Tag        Type                         Name/Value
 0x0000000000000001 (NEEDED)             Shared library: [libgcc_s.so.1]
 0x0000000000000001 (NEEDED)             Shared library: [libc.so.6]
 0x000000000000000c (INIT)               0x3000
"""


@pytest.fixture(scope="module")
def guard():
    spec = importlib.util.spec_from_file_location("verify_static_link", SCRIPT)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules["verify_static_link"] = module
    spec.loader.exec_module(module)
    return module


def test_a_static_binary_is_accepted(guard):
    assert guard.is_statically_linked(STATIC_OUTPUT) is True


def test_a_dynamic_binary_is_rejected(guard):
    assert guard.is_statically_linked(DYNAMIC_OUTPUT) is False


def test_detection_tolerates_spacing_and_case_drift(guard):
    # binutils wording varies across versions; the check normalizes rather
    # than matching one exact byte sequence, or a future binutils could turn
    # this guard into a silent no-op.
    assert (
        guard.is_statically_linked("THERE IS NO   Dynamic Section In This File.")
        is True
    )


def test_empty_output_is_not_treated_as_static(guard):
    # The dangerous default. If readelf produced nothing, we know nothing --
    # and "unknown" must never read as "verified static".
    assert guard.is_statically_linked("") is False


def test_failure_names_the_offending_libraries(guard):
    needed = guard.dynamic_dependencies(DYNAMIC_OUTPUT)
    assert needed == ["libgcc_s.so.1", "libc.so.6"], needed


def test_no_needed_entries_yields_no_names(guard):
    assert guard.dynamic_dependencies(STATIC_OUTPUT) == []


def test_main_fails_when_the_binary_cannot_be_inspected(guard, tmp_path, capsys):
    # A missing path must fail, not pass by default: a verification step that
    # silently skips is worse than no step at all.
    code = guard.main([str(tmp_path / "definitely-not-here")])
    assert code == 1
    assert "cannot inspect" in capsys.readouterr().err
