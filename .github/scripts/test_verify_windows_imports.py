"""Tests for the Windows import ratchet.

Windows was the last release surface with no portability guard, and it has the
same failure mode as the others: a binary importing a DLL the machine lacks
does not degrade, it refuses to start.

Measured on published v0.8.29, `soldr.exe` and `soldr-daemon.exe` import
exactly one non-system DLL, `VCRUNTIME140.dll`, which comes from the Visual
C++ Redistributable rather than from Windows.

PE images are assembled here with `struct.pack` so the parser is exercised
without shipping a binary fixture.
"""

from __future__ import annotations

import importlib.util
import struct
import sys
from pathlib import Path

import pytest

SCRIPT = Path(__file__).resolve().parent / "verify_windows_imports.py"

# What v0.8.29's soldr.exe actually imports.
REAL_IMPORTS = [
    "VCRUNTIME140.dll",
    "advapi32.dll",
    "api-ms-win-core-synch-l1-2-0.dll",
    "api-ms-win-crt-heap-l1-1-0.dll",
    "api-ms-win-crt-runtime-l1-1-0.dll",
    "bcrypt.dll",
    "bcryptprimitives.dll",
    "combase.dll",
    "crypt32.dll",
    "kernel32.dll",
    "ntdll.dll",
    "pdh.dll",
    "powrprof.dll",
    "shell32.dll",
    "userenv.dll",
    "ws2_32.dll",
]


@pytest.fixture(scope="module")
def mod():
    spec = importlib.util.spec_from_file_location("verify_windows_imports", SCRIPT)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules["verify_windows_imports"] = module
    spec.loader.exec_module(module)
    return module


def _pe(
    dll_names: "list[str]", magic: int = 0x20B, import_rva: "int | None" = None
) -> bytes:
    """Assemble a minimal but structurally valid PE32+ with an import table.

    Layout: DOS header -> PE signature -> COFF -> optional header (with data
    directories) -> one section header -> section data holding the import
    descriptors and the DLL name strings.
    """
    section_rva = 0x1000
    opt_size = 240 if magic == 0x20B else 224

    dos = b"MZ" + b"\0" * 0x3A + struct.pack("<I", 0x40)
    coff = struct.pack("<HHIIIHH", 0x8664, 1, 0, 0, 0, opt_size, 0x22)

    # Optional header: magic then padding up to the data directories.
    dir_start = 112 if magic == 0x20B else 96
    opt = struct.pack("<H", magic) + b"\0" * (dir_start - 2)
    directories = b"\0" * 8  # entry 0: export
    # Import descriptors sit at the start of the section.
    descriptors_rva = section_rva if import_rva is None else import_rva
    directories += struct.pack("<II", descriptors_rva, 0)
    directories += b"\0" * (opt_size - dir_start - 16)
    opt = opt + directories

    # Build the section payload: N descriptors + terminator, then the names.
    descriptor_bytes = b""
    name_blob = b""
    names_rva = section_rva + 20 * (len(dll_names) + 1)
    for dll in dll_names:
        name_rva = names_rva + len(name_blob)
        descriptor_bytes += struct.pack("<IIIII", 0, 0, 0, name_rva, 0)
        name_blob += dll.encode("ascii") + b"\0"
    descriptor_bytes += b"\0" * 20
    payload = descriptor_bytes + name_blob

    raw_pointer = 0x400
    section = struct.pack(
        "<8sIIIIIIHHI",
        b".rdata\0\0",
        len(payload),
        section_rva,
        len(payload),
        raw_pointer,
        0,
        0,
        0,
        0,
        0x40000040,
    )

    head = dos + b"PE\0\0" + coff + opt + section
    return head + b"\0" * (raw_pointer - len(head)) + payload


# --- classification -------------------------------------------------------


def test_the_real_import_set_is_accepted(mod):
    assert mod.unexpected_imports(REAL_IMPORTS) == []


def test_api_set_forwarders_count_as_system(mod):
    assert mod.is_system_dll("api-ms-win-crt-stdio-l1-1-0.dll") is True
    assert mod.is_system_dll("ext-ms-win-something.dll") is True


def test_dll_matching_is_case_insensitive(mod):
    # Import tables carry mixed case: v0.8.29 shows VCRUNTIME140.dll next to
    # a lowercase kernel32.dll in the same binary.
    assert mod.is_system_dll("KERNEL32.DLL") is True
    assert mod.is_redistributable_dll("VCRUNTIME140.dll") is True
    assert mod.is_redistributable_dll("vcruntime140.dll") is True


def test_the_redistributable_is_allowed_but_identified(mod):
    # Allowed, because it is what ships today. Identified, because every
    # machine running this binary needs the redist installed.
    assert mod.is_redistributable_dll("vcruntime140.dll") is True
    assert mod.is_system_dll("vcruntime140.dll") is False


def test_an_unknown_dll_is_reported(mod):
    found = mod.unexpected_imports([*REAL_IMPORTS, "libssl-3-x64.dll"])
    assert found == ["libssl-3-x64.dll"]


def test_the_same_dll_in_two_cases_is_one_problem_not_two(mod):
    # DLL names are case-insensitive on Windows, so `aaa.dll` and `AAA.dll`
    # are the same file. Reporting both would read as two separate missing
    # dependencies. The first spelling seen is kept so the message matches
    # what is actually in the import table.
    assert mod.unexpected_imports(["zzz.dll", "aaa.dll", "AAA.dll"]) == [
        "aaa.dll",
        "zzz.dll",
    ]
    assert mod.unexpected_imports(["AAA.dll", "aaa.dll"]) == ["AAA.dll"]


# --- PE parsing -----------------------------------------------------------


def test_imports_are_read_from_a_pe32_plus(mod):
    assert mod.pe_imports(_pe(["kernel32.dll", "VCRUNTIME140.dll"])) == [
        "kernel32.dll",
        "VCRUNTIME140.dll",
    ]


def test_imports_are_read_from_a_pe32(mod):
    assert mod.pe_imports(_pe(["kernel32.dll"], magic=0x10B)) == ["kernel32.dll"]


def test_a_non_pe_is_an_error(mod):
    with pytest.raises(mod.PEError, match="MZ"):
        mod.pe_imports(b"\x7fELF" + b"\0" * 128)


def test_a_missing_pe_signature_is_an_error(mod):
    data = bytearray(_pe(["kernel32.dll"]))
    data[0x40:0x44] = b"XXXX"
    with pytest.raises(mod.PEError, match="signature"):
        mod.pe_imports(bytes(data))


def test_an_unknown_optional_header_magic_is_an_error(mod):
    with pytest.raises(mod.PEError, match="magic"):
        mod.pe_imports(_pe(["kernel32.dll"], magic=0x999))


def test_no_import_directory_is_an_error_not_an_empty_list(mod):
    # An empty list would read as "no dependencies" -- the most reassuring
    # possible answer and the least likely to be true.
    with pytest.raises(mod.PEError, match="no import directory"):
        mod.pe_imports(_pe(["kernel32.dll"], import_rva=0))


def test_an_import_directory_with_no_dlls_is_an_error(mod):
    # A well-formed directory that happens to list nothing. Returning [] here
    # would report "no dependencies" -- the most reassuring possible answer,
    # and for a real soldr.exe certainly wrong.
    with pytest.raises(mod.PEError, match="no DLLs"):
        mod.pe_imports(_pe([]))


def test_an_unmappable_rva_is_an_error(mod):
    with pytest.raises(mod.PEError, match="cannot map RVA"):
        mod.pe_imports(_pe(["kernel32.dll"], import_rva=0xDEAD0000))


def test_a_truncated_file_is_an_error(mod):
    with pytest.raises(mod.PEError):
        mod.pe_imports(b"MZ")


# --- the ratchet ----------------------------------------------------------


def _write(tmp_path: Path, name: str, data: bytes) -> str:
    path = tmp_path / name
    path.write_bytes(data)
    return str(path)


def test_a_binary_with_the_current_imports_passes(mod, tmp_path):
    binary = _write(tmp_path, "soldr.exe", _pe(REAL_IMPORTS))
    assert mod.main([binary]) == 0


def test_a_new_non_system_dependency_fails(mod, tmp_path):
    binary = _write(tmp_path, "soldr.exe", _pe([*REAL_IMPORTS, "libpq.dll"]))
    assert mod.main([binary]) == 1


def test_an_unreadable_binary_fails(mod, tmp_path):
    assert mod.main([str(tmp_path / "missing.exe")]) == 1


def test_an_unparseable_binary_fails(mod, tmp_path):
    binary = _write(tmp_path, "soldr.exe", b"\x7fELF" + b"\0" * 128)
    assert mod.main([binary]) == 1


def test_every_binary_is_checked_not_just_the_first(mod, tmp_path):
    good = _write(tmp_path, "soldr.exe", _pe(REAL_IMPORTS))
    bad = _write(tmp_path, "soldr-daemon.exe", _pe([*REAL_IMPORTS, "evil.dll"]))
    assert mod.main([good, bad]) == 1


def test_the_redistributable_is_named_in_the_success_line(mod, tmp_path, capsys):
    # A pass should still say the redist is required, or the cost disappears.
    binary = _write(tmp_path, "soldr.exe", _pe(REAL_IMPORTS))
    assert mod.main([binary]) == 0
    assert "VCRUNTIME140.dll" in capsys.readouterr().out
