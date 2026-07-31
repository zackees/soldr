"""The musl artifacts must really be statically linked (soldr#1060).

A musl target that silently picks up a dynamic link still builds and still
passes tests on the modern CI image — it only fails on the old distro the
artifact exists to serve. These tests pin the detection so the guard cannot
quietly become a no-op.
"""

from __future__ import annotations

from pathlib import Path

import pytest
from conftest import load_script_module

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
    return load_script_module(SCRIPT, "verify_static_link")


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
    #
    # Deliberately platform-independent. Originally this asserted only the
    # missing-*tool* wording, which passed on a Windows box (no readelf on
    # PATH) and failed on Linux, where readelf exists and exits non-zero on a
    # missing path. Both routes mean "cannot verify", so both must say so.
    code = guard.main([str(tmp_path / "definitely-not-here")])
    assert code == 1
    err = capsys.readouterr().err
    assert "cannot inspect" in err, err
    assert "definitely-not-here" in err, err


# --- static-PIE (soldr#1060 follow-up) ------------------------------------
#
# The published musl `crgx` and `cargo-chef` are static-PIE: a `.dynamic`
# section with 20 entries, zero NEEDED, and no INTERP. `file` calls them
# "static-pie linked" and they run on Debian 12 and Alpine alike. The
# original "no dynamic section" test called them dynamic, which is why only
# `soldr` was ever verified -- extending the check to the whole bundle would
# have failed the release on correct artifacts.

STATIC_PIE_DYNAMIC = """
Dynamic section at offset 0x3b0750 contains 20 entries:
  Tag        Type                         Name/Value
 0x000000000000000c (INIT)               0x22000
 0x000000000000000d (FINI)               0x2b108a
 0x0000000000000019 (INIT_ARRAY)         0x39b290
"""

STATIC_PIE_HEADERS = """
Program Headers:
  Type           Offset             VirtAddr           PhysAddr
  LOAD           0x0000000000000000 0x0000000000000000 0x0000000000000000
  DYNAMIC        0x00000000003b0750 0x00000000003b1750 0x00000000003b1750
"""

DYNAMIC_HEADERS = """
Program Headers:
  Type           Offset             VirtAddr           PhysAddr
  INTERP         0x0000000000000318 0x0000000000000318 0x0000000000000318
      [Requesting program interpreter: /lib64/ld-linux-x86-64.so.2]
  LOAD           0x0000000000000000 0x0000000000000000 0x0000000000000000
"""


def test_static_pie_is_accepted(guard):
    assert guard.is_statically_linked(STATIC_PIE_DYNAMIC, STATIC_PIE_HEADERS) is True


def test_an_interpreter_is_rejected_even_with_no_needed_entries(guard):
    # A binary can name an interpreter without listing NEEDED libraries. It
    # still cannot start where that loader is absent, so INTERP alone decides.
    assert guard.is_statically_linked(STATIC_PIE_DYNAMIC, DYNAMIC_HEADERS) is False


def test_needed_entries_are_still_rejected(guard):
    assert guard.is_statically_linked(DYNAMIC_OUTPUT, STATIC_PIE_HEADERS) is False


def test_empty_program_headers_do_not_make_a_dynamic_binary_pass(guard):
    assert guard.is_statically_linked(DYNAMIC_OUTPUT, "") is False


def test_blank_readelf_output_is_never_static(guard):
    # "found no NEEDED entries" and "read nothing at all" must not reach the
    # same answer; otherwise a broken invocation reports everything as static.
    assert guard.is_statically_linked("", STATIC_PIE_HEADERS) is False
    assert guard.is_statically_linked("   \n  ", STATIC_PIE_HEADERS) is False


def test_interpreter_detection_is_direct(guard):
    assert guard.requires_interpreter(DYNAMIC_HEADERS) is True
    assert guard.requires_interpreter(STATIC_PIE_HEADERS) is False
