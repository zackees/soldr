from __future__ import annotations

import json
import os
import shutil
import subprocess
import zipfile
from pathlib import Path

from ci.env import host_target_triple
from ci.tiny_pdb_symbols import ROOT, TINY_PDB_SYMBOLS, filter_list_contents
from ci.wheel_record import record_entry, render_record

_GLOBAL_RELEASE_RUSTFLAGS = (
    "-C force-frame-pointers=yes",
    "-C force-unwind-tables=yes",
    "-C debuginfo=0",
)


def apply_tiny_pdb_env(env: dict[str, str]) -> dict[str, str]:
    rendered = " ".join(_GLOBAL_RELEASE_RUSTFLAGS)
    current = env.get("RUSTFLAGS", "").strip()
    updated = env.copy()
    updated["RUSTFLAGS"] = f"{current} {rendered}".strip() if current else rendered
    return updated


def stripped_pdb_path(root: Path = ROOT) -> Path:
    return root / "target" / "tiny-pdb" / host_target_triple() / "_native.stripped.pdb"


def filtered_pdb_path(root: Path = ROOT) -> Path:
    return root / "target" / "tiny-pdb" / host_target_triple() / "_native.tiny.pdb"


def final_crate_rustc_args(root: Path = ROOT) -> list[str]:
    stripped = stripped_pdb_path(root)
    stripped.parent.mkdir(parents=True, exist_ok=True)
    return [
        "-Cdebuginfo=line-tables-only",
        f"-Clink-arg=/PDBSTRIPPED:{stripped}",
    ]


def resolve_pdbcopy() -> str:
    kits_roots = [
        Path(os.environ.get("ProgramFiles(x86)", r"C:\Program Files (x86)")),
        Path(os.environ.get("ProgramFiles", r"C:\Program Files")),
    ]
    candidates = [shutil.which("pdbcopy")]
    for kits_root in kits_roots:
        for arch in ("x64", "arm64", "x86"):
            candidates.append(
                str(
                    kits_root
                    / "Windows Kits"
                    / "10"
                    / "Debuggers"
                    / arch
                    / "pdbcopy.exe"
                )
            )
    for candidate in candidates:
        if candidate and Path(candidate).exists():
            return str(candidate)
    raise RuntimeError("pdbcopy.exe was not found")


def write_filter_file(root: Path = ROOT) -> Path:
    path = root / "target" / "tiny-pdb" / "public-symbols.txt"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(filter_list_contents(), encoding="utf-8")
    return path


def filter_public_pdb(
    *,
    source_pdb: Path,
    destination_pdb: Path,
    root: Path = ROOT,
) -> Path:
    filter_file = write_filter_file(root)
    destination_pdb.parent.mkdir(parents=True, exist_ok=True)
    result = subprocess.run(
        [
            resolve_pdbcopy(),
            str(source_pdb),
            str(destination_pdb),
            f"-F:@{filter_file}",
        ],
        capture_output=True,
        text=True,
        check=False,
        encoding="utf-8",
    )
    if result.returncode != 0:
        raise RuntimeError(
            f"pdbcopy failed for {source_pdb} -> {destination_pdb}:\n"
            f"{result.stdout}\n{result.stderr}"
        )
    return destination_pdb


def _native_extension_entry(wheel: Path) -> str:
    with zipfile.ZipFile(wheel) as zf:
        for name in zf.namelist():
            lower = name.lower()
            if lower.startswith("running_process/") and lower.endswith(".pyd"):
                return name
    raise RuntimeError(
        f"could not find running_process native extension inside {wheel}"
    )


def _replace_wheel_entries(wheel: Path, replacements: dict[str, bytes]) -> None:
    temp_path = wheel.with_suffix(".tmp.whl")
    record_name = record_entry(wheel)
    try:
        with (
            zipfile.ZipFile(wheel) as src,
            zipfile.ZipFile(temp_path, "w", compression=zipfile.ZIP_DEFLATED) as dst,
        ):
            replacement_names = set(replacements)
            record_rows: list[tuple[str, bytes]] = []
            for info in src.infolist():
                if info.filename in replacement_names or info.filename == record_name:
                    continue
                payload = src.read(info.filename)
                dst.writestr(info, payload)
                record_rows.append((info.filename, payload))
            for name, payload in replacements.items():
                dst.writestr(name, payload)
                record_rows.append((name, payload))
            dst.writestr(record_name, render_record(record_rows, record_name))
        temp_path.replace(wheel)
    finally:
        if temp_path.exists():
            temp_path.unlink()


def bundle_windows_tiny_pdb(
    wheel: Path,
    *,
    tiny_pdb: Path,
    root: Path = ROOT,
) -> list[str]:
    native_entry = _native_extension_entry(wheel)
    pdb_entry = native_entry.removesuffix(".pyd") + ".pdb"
    manifest_entry = native_entry.removesuffix(".pyd") + ".tiny-pdb.json"
    manifest = {
        "schema_version": 1,
        "symbols": [spec.__dict__ for spec in TINY_PDB_SYMBOLS],
    }
    replacements = {
        pdb_entry: tiny_pdb.read_bytes(),
        manifest_entry: json.dumps(manifest, indent=2, sort_keys=True).encode("utf-8"),
    }
    _replace_wheel_entries(wheel, replacements)
    return sorted(replacements)
