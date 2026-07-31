"""Tests for the wheel-content glibc check.

The existing release gate asserts the wheel *filename* carries
`manylinux_2_17`. This one asserts the bytes agree, because the two failures
are not equally visible:

* tagged 2.39, actually fine -> pip skips the wheel. Loud.
* tagged 2.17, actually 2.39 -> pip installs it, because the tag promises it
  works, and the program dies at run time. Quiet, and downstream.

Verified against the real published artifacts while writing this: the
v0.8.29 manylinux wheel's embedded soldr needs at most GLIBC_2.17 and runs on
Debian 12, while the tarball binary from the same release requires 2.39.
"""

from __future__ import annotations

import zipfile
from pathlib import Path

import pytest
from _script_loader import load_script_module

SCRIPT = Path(__file__).resolve().parent / "verify_wheel_glibc.py"

NEEDS_2_17 = "  0x0030:   Name: GLIBC_2.17  Flags: none  Version: 7\n"
NEEDS_2_39 = "  0x0030:   Name: GLIBC_2.39  Flags: none  Version: 5\n"
NO_VERSIONS = "There is no version information in this file.\n"


@pytest.fixture(scope="module")
def mod():
    return load_script_module(SCRIPT, "verify_wheel_glibc")


def _wheel(path: Path, entries: "dict[str, bytes]") -> Path:
    with zipfile.ZipFile(path, "w") as archive:
        for name, data in entries.items():
            archive.writestr(name, data)
    return path


def _standard_wheel(tmp_path: Path, name: str = "soldr-1.2.3-py3-none-any.whl") -> Path:
    return _wheel(
        tmp_path / name,
        {
            "soldr-1.2.3.data/scripts/soldr": b"\x7fELF fake",
            "soldr-1.2.3.dist-info/METADATA": b"Name: soldr\n",
            "soldr-1.2.3.dist-info/RECORD": b"",
        },
    )


# --- locating the embedded binary ----------------------------------------


def test_finds_the_binary_in_the_maturin_scripts_layout(mod, tmp_path):
    wheel = _standard_wheel(tmp_path)
    found = mod.embedded_binaries(wheel, tmp_path / "x")
    assert [p.name for p in found] == ["soldr"]


def test_metadata_files_are_not_mistaken_for_binaries(mod, tmp_path):
    # RECORD and METADATA live in `.dist-info`, not `.data/scripts`. Matching
    # on the layout rather than on a name keeps them out.
    wheel = _standard_wheel(tmp_path)
    found = mod.embedded_binaries(wheel, tmp_path / "x")
    assert all("dist-info" not in str(p) for p in found)


def test_a_scripts_dir_outside_dot_data_is_not_a_binary(mod, tmp_path):
    # `.data/scripts/` is the maturin layout. A `scripts/` directory anywhere
    # else is not, and matching on the parent name alone would sweep it in --
    # which would then be handed to readelf and reported as a result.
    wheel = _wheel(
        tmp_path / "soldr-1.2.3-py3-none-any.whl",
        {
            "soldr-1.2.3.data/scripts/soldr": b"\x7fELF fake",
            "soldr-1.2.3.dist-info/scripts/README": b"not a binary\n",
        },
    )
    found = mod.embedded_binaries(wheel, tmp_path / "x")
    assert [p.name for p in found] == ["soldr"]


def test_multiple_scripts_are_all_returned(mod, tmp_path):
    wheel = _wheel(
        tmp_path / "soldr-1.2.3-py3-none-any.whl",
        {
            "soldr-1.2.3.data/scripts/soldr": b"a",
            "soldr-1.2.3.data/scripts/soldr-daemon": b"b",
        },
    )
    found = sorted(p.name for p in mod.embedded_binaries(wheel, tmp_path / "x"))
    assert found == ["soldr", "soldr-daemon"]


# --- the decision ---------------------------------------------------------


def _patch_readelf(mod, monkeypatch, output: str, code: int = 0):
    baseline = mod._load_baseline_module()
    monkeypatch.setattr(mod, "_load_baseline_module", lambda: baseline, raising=True)
    monkeypatch.setattr(
        baseline, "_readelf_versions", lambda binary: (code, output), raising=True
    )


def test_a_wheel_within_the_floor_passes(mod, monkeypatch, tmp_path):
    _patch_readelf(mod, monkeypatch, NEEDS_2_17)
    wheel = _standard_wheel(tmp_path)
    assert mod.main(["--max-glibc", "2.17", str(wheel)]) == 0


def test_a_mis_tagged_wheel_fails(mod, monkeypatch, tmp_path):
    # The whole point: tag says 2.17, bytes say 2.39.
    _patch_readelf(mod, monkeypatch, NEEDS_2_39)
    wheel = _standard_wheel(tmp_path)
    assert mod.main(["--max-glibc", "2.17", str(wheel)]) == 1


def test_a_static_binary_passes(mod, monkeypatch, tmp_path):
    _patch_readelf(mod, monkeypatch, NO_VERSIONS)
    wheel = _standard_wheel(tmp_path)
    assert mod.main(["--max-glibc", "2.17", str(wheel)]) == 0


def test_a_wheel_with_no_embedded_binary_fails(mod, monkeypatch, tmp_path):
    # Not a pass. Either the layout changed or maturin shipped no binary, and
    # both mean this gate silently stopped gating.
    _patch_readelf(mod, monkeypatch, NEEDS_2_17)
    wheel = _wheel(
        tmp_path / "soldr-1.2.3-py3-none-any.whl",
        {"soldr-1.2.3.dist-info/METADATA": b"Name: soldr\n"},
    )
    assert mod.main(["--max-glibc", "2.17", str(wheel)]) == 1


def test_an_empty_wheel_alongside_a_good_one_still_fails(mod, monkeypatch, tmp_path):
    # The single-wheel case above is also caught by the "nothing was checked"
    # backstop, so it cannot tell the explicit branch from the backstop. Here
    # a real binary IS checked, so only the per-wheel branch can fail this.
    _patch_readelf(mod, monkeypatch, NEEDS_2_17)
    good = _standard_wheel(tmp_path, "good-1.2.3-py3-none-any.whl")
    empty = _wheel(
        tmp_path / "empty-1.2.3-py3-none-any.whl",
        {"empty-1.2.3.dist-info/METADATA": b"Name: empty\n"},
    )
    assert mod.main(["--max-glibc", "2.17", str(good), str(empty)]) == 1


def test_a_missing_wheel_fails(mod, monkeypatch, tmp_path):
    _patch_readelf(mod, monkeypatch, NEEDS_2_17)
    assert mod.main(["--max-glibc", "2.17", str(tmp_path / "nope.whl")]) == 1


def test_a_corrupt_wheel_fails(mod, monkeypatch, tmp_path):
    _patch_readelf(mod, monkeypatch, NEEDS_2_17)
    bad = tmp_path / "broken.whl"
    bad.write_bytes(b"not a zip")
    assert mod.main(["--max-glibc", "2.17", str(bad)]) == 1


def test_an_unreadable_binary_fails_rather_than_passes(mod, monkeypatch, tmp_path):
    _patch_readelf(mod, monkeypatch, "readelf: Error", code=1)
    wheel = _standard_wheel(tmp_path)
    assert mod.main(["--max-glibc", "2.17", str(wheel)]) == 1


def test_every_wheel_is_checked_not_just_the_first(mod, monkeypatch, tmp_path):
    _patch_readelf(mod, monkeypatch, NEEDS_2_39)
    a = _standard_wheel(tmp_path, "a-1.2.3-py3-none-any.whl")
    b = _standard_wheel(tmp_path, "b-1.2.3-py3-none-any.whl")
    assert mod.main(["--max-glibc", "2.17", str(a), str(b)]) == 1


def test_a_nonsense_ceiling_is_rejected(mod, monkeypatch, tmp_path):
    _patch_readelf(mod, monkeypatch, NEEDS_2_17)
    wheel = _standard_wheel(tmp_path)
    assert mod.main(["--max-glibc", "nope", str(wheel)]) == 1


def test_the_default_ceiling_is_the_manylinux_floor(mod, monkeypatch, tmp_path):
    # Default 2.17 so the tag and the check agree without CI restating it.
    _patch_readelf(mod, monkeypatch, NEEDS_2_39)
    wheel = _standard_wheel(tmp_path)
    assert mod.main([str(wheel)]) == 1
