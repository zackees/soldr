from __future__ import annotations

import json
import zipfile
from pathlib import Path

from ci import tiny_pdb
from ci.tiny_pdb_symbols import TINY_PDB_SYMBOLS
from ci.wheel_record import validate_record


def test_apply_tiny_pdb_env_appends_required_rustflags() -> None:
    env = tiny_pdb.apply_tiny_pdb_env({"RUSTFLAGS": "-C target-feature=+crt-static"})

    assert "-C target-feature=+crt-static" in env["RUSTFLAGS"]
    assert "-C force-frame-pointers=yes" in env["RUSTFLAGS"]
    assert "-C force-unwind-tables=yes" in env["RUSTFLAGS"]
    assert "-C debuginfo=0" in env["RUSTFLAGS"]


def test_final_crate_rustc_args_emit_line_tables_and_stripped_pdb_path(
    tmp_path: Path,
) -> None:
    args = tiny_pdb.final_crate_rustc_args(tmp_path)

    assert "-Cdebuginfo=line-tables-only" in args
    stripped_arg = next(
        arg for arg in args if arg.startswith("-Clink-arg=/PDBSTRIPPED:")
    )
    stripped_path = Path(stripped_arg.removeprefix("-Clink-arg=/PDBSTRIPPED:"))
    assert stripped_path == tiny_pdb.stripped_pdb_path(tmp_path)
    assert stripped_path.parent.is_dir()


def test_write_filter_file_contains_allowlisted_public_symbols(tmp_path: Path) -> None:
    path = tiny_pdb.write_filter_file(tmp_path)

    assert path.read_text(encoding="utf-8").splitlines() == [
        spec.name for spec in TINY_PDB_SYMBOLS
    ]


def test_resolve_pdbcopy_checks_arm64_debugger_path(
    monkeypatch, tmp_path: Path
) -> None:
    kits_root = tmp_path / "Program Files"
    arm64_pdbcopy = (
        kits_root / "Windows Kits" / "10" / "Debuggers" / "arm64" / "pdbcopy.exe"
    )
    arm64_pdbcopy.parent.mkdir(parents=True, exist_ok=True)
    arm64_pdbcopy.write_text("stub", encoding="utf-8")

    monkeypatch.setattr(tiny_pdb.shutil, "which", lambda _: None)
    monkeypatch.setenv("ProgramFiles(x86)", str(tmp_path / "missing"))
    monkeypatch.setenv("ProgramFiles", str(kits_root))

    assert tiny_pdb.resolve_pdbcopy() == str(arm64_pdbcopy)


def test_bundle_windows_tiny_pdb_injects_pdb_and_manifest(tmp_path: Path) -> None:
    wheel = tmp_path / "running_process-3.0.3-cp313-cp313-win_amd64.whl"
    pdb = tmp_path / "_native.tiny.pdb"
    pdb.write_bytes(b"pdb-bytes")
    record_entry = "running_process-3.0.3.dist-info/RECORD"
    with zipfile.ZipFile(wheel, "w") as zf:
        zf.writestr("running_process/__init__.py", "__version__ = '3.0.3'\n")
        zf.writestr("running_process/_native.cp313-win_amd64.pyd", b"native-binary")
        zf.writestr(record_entry, f"{record_entry},,\n")

    bundled = tiny_pdb.bundle_windows_tiny_pdb(wheel, tiny_pdb=pdb, root=tmp_path)

    assert bundled == [
        "running_process/_native.cp313-win_amd64.pdb",
        "running_process/_native.cp313-win_amd64.tiny-pdb.json",
    ]
    with zipfile.ZipFile(wheel) as zf:
        assert zf.read("running_process/_native.cp313-win_amd64.pdb") == b"pdb-bytes"
        manifest = json.loads(
            zf.read("running_process/_native.cp313-win_amd64.tiny-pdb.json")
        )
    assert manifest == {
        "schema_version": 1,
        "symbols": [spec.__dict__ for spec in TINY_PDB_SYMBOLS],
    }
    validate_record(wheel)
