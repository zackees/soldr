"""Tests for the macOS minimum-version ratchet.

Same failure shape as the glibc floor on Linux, on a surface nothing watched:
a Mach-O records the oldest macOS it will start on, and if that drifts upward
the binary still builds, still passes CI, and refuses to launch on older Macs.

Measured on published v0.8.29: aarch64 binaries and x86_64 `soldr` demand
macOS 11.0, while x86_64 `crgx` in the same bundle demands only 10.12 -- so
the value is a toolchain default nobody pinned, not a decision.

Mach-O headers are built here with `struct.pack` so the parser is exercised
without needing a Mac or a real binary.
"""

from __future__ import annotations

import importlib.util
import struct
import sys
from pathlib import Path

import pytest

SCRIPT = Path(__file__).resolve().parent / "verify_macos_min_version.py"

MH_MAGIC_64 = 0xFEEDFACF
MH_CIGAM_64 = 0xCFFAEDFE
LC_VERSION_MIN_MACOSX = 0x24
LC_BUILD_VERSION = 0x32


@pytest.fixture(scope="module")
def mod():
    spec = importlib.util.spec_from_file_location("verify_macos_min_version", SCRIPT)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules["verify_macos_min_version"] = module
    spec.loader.exec_module(module)
    return module


def _pack_version(major: int, minor: int, patch: int = 0) -> int:
    return (major << 16) | (minor << 8) | patch


def _macho(commands: "list[bytes]", big_endian: bool = False, ncmds: int = -1) -> bytes:
    # The magic's byte order on disk is what signals the file's endianness.
    # A big-endian Mach-O writes MH_MAGIC_64 big-endian, i.e. bytes
    # FE ED FA CF -- which a little-endian read then sees as MH_CIGAM_64.
    # Packing MH_CIGAM_64 itself would produce a little-endian file.
    endian = ">" if big_endian else "<"
    header = struct.pack(
        endian + "IiiIIIII",
        MH_MAGIC_64,
        0x0100000C,  # cputype arm64
        0,  # cpusubtype
        2,  # filetype MH_EXECUTE
        len(commands) if ncmds < 0 else ncmds,
        sum(len(c) for c in commands),
        0,
        0,
    )
    return header + b"".join(commands)


def _build_version_cmd(
    major: int, minor: int, patch: int = 0, endian: str = "<"
) -> bytes:
    return struct.pack(
        endian + "IIIIII",
        LC_BUILD_VERSION,
        24,
        1,  # platform: macOS
        _pack_version(major, minor, patch),
        _pack_version(15, 5),  # sdk
        0,  # ntools
    )


def _version_min_cmd(
    major: int, minor: int, patch: int = 0, endian: str = "<"
) -> bytes:
    return struct.pack(
        endian + "IIII",
        LC_VERSION_MIN_MACOSX,
        16,
        _pack_version(major, minor, patch),
        _pack_version(15, 5),
    )


def _filler_cmd(endian: str = "<") -> bytes:
    # An unrelated load command the parser must skip over rather than trip on.
    return struct.pack(endian + "II", 0x19, 16) + b"\x00" * 8


# --- version decoding -----------------------------------------------------


@pytest.mark.parametrize(
    "packed,expected",
    [((11 << 16), (11, 0, 0)), ((10 << 16) | (12 << 8), (10, 12, 0))],
)
def test_version_decoding(mod, packed, expected):
    assert mod.decode_version(packed) == expected


def test_a_ceiling_without_a_patch_matches_an_exact_minimum(mod):
    # `(11, 0) < (11, 0, 0)` as plain tuples, so a ceiling written "11.0"
    # would reject a binary whose minimum is exactly 11.0.0 -- failing the
    # release for matching the ceiling it was handed.
    assert mod._padded(mod.parse_version("11.0")) == (11, 0, 0)
    assert mod._padded((11, 0, 0)) == (11, 0, 0)


# --- parsing --------------------------------------------------------------


def test_reads_lc_build_version(mod):
    assert mod.minimum_os(_macho([_build_version_cmd(11, 0)])) == (11, 0, 0)


def test_reads_the_older_lc_version_min_macosx(mod):
    # x86_64 crgx in the published bundle still uses this older command.
    assert mod.minimum_os(_macho([_version_min_cmd(10, 12)])) == (10, 12, 0)


def test_skips_unrelated_load_commands(mod):
    data = _macho([_filler_cmd(), _filler_cmd(), _build_version_cmd(12, 3, 1)])
    assert mod.minimum_os(data) == (12, 3, 1)


def test_big_endian_mach_o_is_understood(mod):
    data = _macho([_build_version_cmd(11, 0, endian=">")], big_endian=True)
    assert mod.minimum_os(data) == (11, 0, 0)


# --- the things that must never quietly pass ------------------------------


def test_a_macho_without_a_minimum_command_is_an_error(mod):
    # "Found nothing" must not read as "fine".
    with pytest.raises(mod.MachOError):
        mod.minimum_os(_macho([_filler_cmd()]))


def test_a_universal_binary_is_an_error_not_a_pass(mod):
    # A fat binary would otherwise parse as garbage. It has to be recognised
    # so it fails as "cannot verify".
    with pytest.raises(mod.MachOError, match="universal"):
        mod.minimum_os(struct.pack(">I", 0xCAFEBABE) + b"\x00" * 64)


def test_a_non_macho_is_an_error(mod):
    with pytest.raises(mod.MachOError):
        mod.minimum_os(b"\x7fELF" + b"\x00" * 64)


def test_a_truncated_file_is_an_error(mod):
    with pytest.raises(mod.MachOError):
        mod.minimum_os(b"\xcf\xfa\xed\xfe")


def test_load_commands_past_the_end_are_an_error(mod):
    # A header claiming more commands than the file holds must not fall
    # through to "no minimum found".
    # Filler only, so the walk actually reaches the end instead of returning
    # on a valid command first.
    lying = _macho([_filler_cmd()], ncmds=99)
    with pytest.raises(mod.MachOError, match="past the end"):
        mod.minimum_os(lying)


def test_a_zero_size_load_command_does_not_loop_forever(mod):
    bad = struct.pack("<II", 0x19, 0)
    with pytest.raises(mod.MachOError):
        mod.minimum_os(_macho([bad]))


# --- the ratchet ----------------------------------------------------------


def _write(tmp_path: Path, name: str, data: bytes) -> str:
    path = tmp_path / name
    path.write_bytes(data)
    return str(path)


def test_a_binary_at_the_ceiling_passes(mod, tmp_path):
    binary = _write(tmp_path, "soldr", _macho([_build_version_cmd(11, 0)]))
    assert mod.main(["--max-min-os", "11.0", binary]) == 0


def test_a_binary_above_the_ceiling_fails(mod, tmp_path):
    binary = _write(tmp_path, "soldr", _macho([_build_version_cmd(12, 0)]))
    assert mod.main(["--max-min-os", "11.0", binary]) == 1


def test_a_binary_below_the_ceiling_passes(mod, tmp_path):
    # x86_64 crgx is this case today, at 10.12 against an 11.0 ceiling.
    binary = _write(tmp_path, "crgx", _macho([_version_min_cmd(10, 12)]))
    assert mod.main(["--max-min-os", "11.0", binary]) == 0


def test_a_patch_level_bump_above_the_ceiling_fails(mod, tmp_path):
    binary = _write(tmp_path, "soldr", _macho([_build_version_cmd(11, 0, 1)]))
    assert mod.main(["--max-min-os", "11.0", binary]) == 1


def test_an_unreadable_binary_fails(mod, tmp_path):
    assert mod.main(["--max-min-os", "11.0", str(tmp_path / "missing")]) == 1


def test_an_unparseable_binary_fails(mod, tmp_path):
    binary = _write(tmp_path, "soldr", b"\x7fELF" + b"\x00" * 64)
    assert mod.main(["--max-min-os", "11.0", binary]) == 1


def test_a_nonsense_ceiling_is_rejected(mod, tmp_path):
    binary = _write(tmp_path, "soldr", _macho([_build_version_cmd(11, 0)]))
    assert mod.main(["--max-min-os", "not-a-version", binary]) == 1


def test_every_binary_is_checked_not_just_the_first(mod, tmp_path):
    good = _write(tmp_path, "good", _macho([_build_version_cmd(11, 0)]))
    bad = _write(tmp_path, "bad", _macho([_build_version_cmd(14, 0)]))
    assert mod.main(["--max-min-os", "11.0", good, bad]) == 1


def test_the_default_ceiling_is_the_measured_value(mod, tmp_path):
    binary = _write(tmp_path, "soldr", _macho([_build_version_cmd(11, 0)]))
    assert mod.main([binary]) == 0
    higher = _write(tmp_path, "next", _macho([_build_version_cmd(11, 1)]))
    assert mod.main([higher]) == 1
