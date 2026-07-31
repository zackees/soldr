"""Tests for the glibc-floor ratchet.

The failure this guards is invisible on every machine that runs CI: a
`-unknown-linux-gnu` binary built on a modern runner carries a modern glibc
requirement and only fails on the old distro the artifact exists to serve.
Measured on the published v0.8.29 artifacts, both x86_64 and aarch64 require
`GLIBC_2.39` against a soldr#1060 target of 2.17.

So the parsing gets pinned carefully, and in particular the two ways this
check could pass for the wrong reason: an unreadable binary, and a parse that
finds nothing and calls it clean.
"""

from __future__ import annotations

from pathlib import Path

import pytest
from _script_loader import load_script_module

SCRIPT = Path(__file__).resolve().parent / "verify_glibc_baseline.py"

# Trimmed from real `readelf -V` output for a release soldr binary.
REAL_OUTPUT = """
Version symbols section '.gnu.version' contains 121 entries:
 Addr: 0x0000000000000abc  Offset: 0x000abc  Link: 4 (.dynsym)

Version needs section '.gnu.version_r' contains 3 entries:
 Addr: 0x0000000000000def  Offset: 0x000def  Link: 5 (.dynstr)
  000000: Version: 1  File: libgcc_s.so.1  Cnt: 2
  0x0010:   Name: GCC_3.0  Flags: none  Version: 8
  0x0020: Version: 1  File: libc.so.6  Cnt: 5
  0x0030:   Name: GLIBC_2.14  Flags: none  Version: 7
  0x0040:   Name: GLIBC_2.2.5  Flags: none  Version: 6
  0x0050:   Name: GLIBC_2.39  Flags: none  Version: 5
  0x0060:   Name: GLIBC_2.34  Flags: none  Version: 4
"""

STATIC_OUTPUT = "There is no version information in this file.\n"


@pytest.fixture(scope="module")
def mod():
    return load_script_module(SCRIPT, "verify_glibc_baseline")


# --- version parsing ------------------------------------------------------


@pytest.mark.parametrize(
    "text,expected",
    [
        ("2.17", (2, 17)),
        ("2.2.5", (2, 2, 5)),
        ("2", (2,)),
        (" 2.39 ", (2, 39)),
        ("2.", (2,)),
    ],
)
def test_version_parsing(mod, text, expected):
    assert mod.parse_version(text) == expected


def test_versions_compare_numerically_not_lexically(mod):
    # The bug this prevents: "2.9" > "2.39" as strings. A lexical compare
    # would call a 2.39 binary compliant against a 2.9 ceiling.
    assert mod.parse_version("2.9") < mod.parse_version("2.39")
    assert mod.parse_version("2.2.5") < mod.parse_version("2.14")


# --- requirement extraction ----------------------------------------------


def test_max_requirement_from_real_output(mod):
    assert mod.max_glibc_requirement(REAL_OUTPUT) == (2, 39)


def test_all_requirements_are_collected_and_sorted(mod):
    assert mod.glibc_requirements(REAL_OUTPUT) == [
        (2, 2, 5),
        (2, 14),
        (2, 34),
        (2, 39),
    ]


def test_non_glibc_version_entries_are_ignored(mod):
    # `GCC_3.0` sits in the same section and must not be read as a glibc
    # requirement.
    assert (3, 0) not in mod.glibc_requirements(REAL_OUTPUT)


def test_a_binary_with_no_version_info_has_no_requirement(mod):
    assert mod.max_glibc_requirement(STATIC_OUTPUT) is None


def test_version_definitions_do_not_count_as_requirements(mod):
    # Inspecting libc itself lists GLIBC_* as *definitions*, not needs. The
    # `Name:` anchor is what keeps those out; without it this would report a
    # requirement that does not exist.
    defs = """
Version definition section '.gnu.version_d' contains 2 entries:
  000000: Rev: 1  Flags: BASE  Index: 1  Cnt: 1
  0x001c: Rev: 1  Flags: none  Index: 2  Cnt: 1
"""
    assert mod.max_glibc_requirement(defs) is None


def test_wording_drift_falls_back_rather_than_reporting_clean(mod):
    # If binutils ever stops printing `Name: GLIBC_x.y`, the precise parse
    # finds nothing. Reporting "no requirement" there would silently turn the
    # check into a pass, so a looser scan takes over.
    drifted = "  requires GLIBC_2.39 from libc.so.6\n"
    assert mod.max_glibc_requirement(drifted) == (2, 39)


# --- the ratchet decision -------------------------------------------------


def _run(mod, monkeypatch, output: str, code: int, ceiling: str) -> int:
    monkeypatch.setattr(mod, "_readelf_versions", lambda binary: (code, output))
    return mod.main(["--max-glibc", ceiling, "fake-binary"])


def test_binary_at_the_ceiling_passes(mod, monkeypatch):
    assert _run(mod, monkeypatch, REAL_OUTPUT, 0, "2.39") == 0


def test_binary_above_the_ceiling_fails(mod, monkeypatch):
    assert _run(mod, monkeypatch, REAL_OUTPUT, 0, "2.17") == 1


def test_binary_below_the_ceiling_passes(mod, monkeypatch):
    assert _run(mod, monkeypatch, REAL_OUTPUT, 0, "2.40") == 0


def test_the_default_ceiling_is_the_rfc_target(mod, monkeypatch):
    # Default 2.17, so anyone running the script by hand is told the truth
    # even though CI passes the current measured floor.
    monkeypatch.setattr(mod, "_readelf_versions", lambda binary: (0, REAL_OUTPUT))
    assert mod.main(["fake-binary"]) == 1


def test_a_static_binary_passes_any_ceiling(mod, monkeypatch):
    assert _run(mod, monkeypatch, STATIC_OUTPUT, 0, "2.17") == 0


def test_an_unreadable_binary_fails_rather_than_passes(mod, monkeypatch):
    # readelf exiting non-zero means we learned nothing. "Cannot verify" must
    # never be reported as "verified" -- that is the wrong-reason pass that
    # verify_static_link had to be fixed for.
    assert _run(mod, monkeypatch, "readelf: Error: No such file", 1, "2.17") == 1


def test_a_missing_readelf_fails_rather_than_passes(mod, monkeypatch):
    def boom(binary):
        raise FileNotFoundError("readelf not found on PATH")

    monkeypatch.setattr(mod, "_readelf_versions", boom)
    assert mod.main(["--max-glibc", "2.17", "fake-binary"]) == 1


def test_a_nonsense_ceiling_is_rejected(mod, monkeypatch):
    monkeypatch.setattr(mod, "_readelf_versions", lambda binary: (0, REAL_OUTPUT))
    assert mod.main(["--max-glibc", "not-a-version", "fake-binary"]) == 1


def test_every_binary_is_reported_not_just_the_first(mod, monkeypatch):
    seen = []

    def fake(binary):
        seen.append(binary)
        return 0, REAL_OUTPUT

    monkeypatch.setattr(mod, "_readelf_versions", fake)
    assert mod.main(["--max-glibc", "2.17", "a", "b", "c"]) == 1
    assert seen == ["a", "b", "c"]
