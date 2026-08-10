from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
import tempfile
import zipfile
from pathlib import Path

from ci.tiny_pdb_symbols import (
    DISALLOWED_PUBLIC_SYMBOL_PATTERNS,
    ROOT,
    TINY_PDB_SYMBOLS,
)
from ci.wheel_record import validate_record

DEFAULT_PDB_LOW_WATER_BYTES = 50_000
# Hard cap on the shipped tiny PDB. The IDEAL target stays at 100 KB —
# anything between IDEAL and HIGH_WATER flips `ideal_size_met=False` so
# the gradual creep is visible — but only HIGH_WATER fails the release.
# Bumped from 1 MB to 2 MB on 4.5.4 to unblock release after the v2
# broker scaffold + vendored-sidecar work legitimately grew the
# allowlisted symbol set past the prior cap (~1.18 MB observed). 2 MB
# leaves ~70% headroom; revisit if the next release also creeps.
DEFAULT_PDB_HIGH_WATER_BYTES = 2_000_000
DEFAULT_PDB_IDEAL_HIGH_WATER_BYTES = 100_000
DEFAULT_COMBINED_NATIVE_HIGH_WATER_BYTES = 8_000_000


def _resolve_llvm_pdbutil(explicit: str | None = None) -> str:
    candidates = [explicit] if explicit else []
    candidates.extend(
        [
            shutil.which("llvm-pdbutil"),
            shutil.which("llvm-pdbutil.exe"),
            str(
                Path(os.environ.get("ProgramFiles", "C:\\Program Files"))
                / "LLVM"
                / "bin"
                / "llvm-pdbutil.exe"
            ),
        ]
    )
    for candidate in candidates:
        if candidate and Path(candidate).exists():
            return str(candidate)
    raise RuntimeError("llvm-pdbutil was not found")


def _run_pdbutil(pdbutil: str, args: list[str], *, pdb_path: Path) -> str:
    result = subprocess.run(
        [pdbutil, *args, str(pdb_path)],
        capture_output=True,
        text=True,
        check=False,
        encoding="utf-8",
    )
    if result.returncode != 0:
        raise RuntimeError(
            f"llvm-pdbutil failed for {pdb_path} with args {args}: {result.stdout}\n{result.stderr}"
        )
    return result.stdout


def wheel_native_entries(wheel: Path) -> dict[str, object]:
    with zipfile.ZipFile(wheel) as zf:
        pyd_entries = [
            name
            for name in zf.namelist()
            if name.startswith("running_process/") and name.endswith(".pyd")
        ]
        if len(pyd_entries) != 1:
            raise RuntimeError(
                f"expected exactly one native extension in {wheel}, found {pyd_entries}"
            )
        pyd_entry = pyd_entries[0]
        pdb_entry = pyd_entry.removesuffix(".pyd") + ".pdb"
        if pdb_entry not in zf.namelist():
            raise RuntimeError(f"expected {pdb_entry} next to {pyd_entry} in {wheel}")
        manifest_entry = pyd_entry.removesuffix(".pyd") + ".tiny-pdb.json"
        if manifest_entry not in zf.namelist():
            raise RuntimeError(
                f"expected {manifest_entry} next to {pyd_entry} in {wheel}"
            )
        pyd_bytes = zf.read(pyd_entry)
        pdb_bytes = zf.read(pdb_entry)
        manifest = json.loads(zf.read(manifest_entry))
    return {
        "pyd_entry": pyd_entry,
        "pdb_entry": pdb_entry,
        "manifest_entry": manifest_entry,
        "pyd_size": len(pyd_bytes),
        "pdb_size": len(pdb_bytes),
        "combined_native_size": len(pyd_bytes) + len(pdb_bytes),
        "manifest": manifest,
        "pdb_bytes": pdb_bytes,
    }


def parse_pdb_summary(summary_text: str) -> dict[str, str]:
    summary: dict[str, str] = {}
    for line in summary_text.splitlines():
        if ":" not in line:
            continue
        key, value = line.split(":", 1)
        summary[key.strip()] = value.strip()
    return summary


def parse_public_symbol_names(publics_text: str) -> list[str]:
    names: list[str] = []
    for line in publics_text.splitlines():
        match = re.search(r"`([^`]+)`", line)
        if match:
            names.append(match.group(1))
    return names


def verify_release_artifact(
    wheel: Path,
    *,
    llvm_pdbutil: str | None = None,
    pdb_low_water_bytes: int = DEFAULT_PDB_LOW_WATER_BYTES,
    pdb_high_water_bytes: int = DEFAULT_PDB_HIGH_WATER_BYTES,
    combined_native_high_water_bytes: int = DEFAULT_COMBINED_NATIVE_HIGH_WATER_BYTES,
) -> dict[str, object]:
    validate_record(wheel)
    entries = wheel_native_entries(wheel)

    pdb_size = int(entries["pdb_size"])
    combined_native_size = int(entries["combined_native_size"])
    if pdb_size < pdb_low_water_bytes:
        raise RuntimeError(
            f"shipped tiny PDB is too small ({pdb_size} bytes < low water {pdb_low_water_bytes})"
        )
    if pdb_size > pdb_high_water_bytes:
        raise RuntimeError(
            f"shipped tiny PDB is too large ({pdb_size} bytes > high water {pdb_high_water_bytes})"
        )
    if combined_native_size > combined_native_high_water_bytes:
        raise RuntimeError(
            "combined native payload is too large "
            f"({combined_native_size} bytes > {combined_native_high_water_bytes})"
        )

    manifest = entries["manifest"]
    manifest_symbols = [item["name"] for item in manifest.get("symbols", [])]
    expected_symbols = [spec.name for spec in TINY_PDB_SYMBOLS]
    if manifest_symbols != expected_symbols:
        raise RuntimeError(
            "tiny PDB manifest does not match the allowlist order/content"
        )

    pdbutil = _resolve_llvm_pdbutil(llvm_pdbutil)
    with tempfile.TemporaryDirectory() as tmpdir:
        pdb_path = Path(tmpdir) / Path(str(entries["pdb_entry"])).name
        pdb_path.write_bytes(entries["pdb_bytes"])
        summary_text = _run_pdbutil(pdbutil, ["dump", "-summary"], pdb_path=pdb_path)
        publics_text = _run_pdbutil(pdbutil, ["dump", "-publics"], pdb_path=pdb_path)

    summary = parse_pdb_summary(summary_text)
    required_summary = {
        "Has Publics": "true",
        "Is stripped": "true",
    }
    summary_errors = [
        f"{key} expected {expected!r}, found {summary.get(key)!r}"
        for key, expected in required_summary.items()
        if summary.get(key) != expected
    ]
    if summary_errors:
        raise RuntimeError(
            "invalid tiny PDB summary:\n- " + "\n- ".join(summary_errors)
        )

    public_symbol_names = parse_public_symbol_names(publics_text)
    if sorted(public_symbol_names) != sorted(expected_symbols):
        raise RuntimeError(
            "tiny PDB public symbol table does not match allowlist exactly:\n"
            f"expected={expected_symbols}\nactual={public_symbol_names}"
        )

    text_lower = publics_text.lower()
    disallowed_hits = [
        pattern
        for pattern in DISALLOWED_PUBLIC_SYMBOL_PATTERNS
        if pattern.lower() in text_lower
    ]
    if disallowed_hits:
        raise RuntimeError(
            "disallowed symbol families leaked into tiny PDB: "
            + ", ".join(disallowed_hits)
        )

    return {
        "wheel": str(wheel),
        "pyd_entry": entries["pyd_entry"],
        "pdb_entry": entries["pdb_entry"],
        "manifest_entry": entries["manifest_entry"],
        "pyd_size": entries["pyd_size"],
        "pdb_size": pdb_size,
        "combined_native_size": combined_native_size,
        "public_symbol_count": len(public_symbol_names),
        "ideal_size_met": pdb_size <= DEFAULT_PDB_IDEAL_HIGH_WATER_BYTES,
        "summary": summary,
    }


def format_release_artifact_report(report: dict[str, object]) -> str:
    ideal_note = "ideal-size=yes" if report["ideal_size_met"] else "ideal-size=no"
    return (
        f"release artifact verified: {Path(str(report['wheel'])).name}; "
        f"{report['pyd_entry']}={report['pyd_size']} bytes; "
        f"{report['pdb_entry']}={report['pdb_size']} bytes; "
        f"combined_native_size={report['combined_native_size']} bytes; "
        f"public_symbols={report['public_symbol_count']}; "
        f"{ideal_note}"
    )


def main(argv: list[str] | None = None) -> int:
    del argv
    wheels = sorted(
        (ROOT / "dist").glob("running_process-*.whl"),
        key=lambda path: path.stat().st_mtime,
    )
    if not wheels:
        raise SystemExit("no wheel found in dist/")
    report = verify_release_artifact(wheels[-1])
    print(format_release_artifact_report(report), flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
