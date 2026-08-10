"""Internal mixed-mode symbolization acceptance for #637.

This is intentionally separate from ``test_live_native_debugger_stack_dump``:
that test proves the public-only debugger surface, while this one feeds the
off-process worker a native frame plus its same-thread Python neighbour.
"""

from __future__ import annotations

import json
import os
import platform
import re
import subprocess
import sys
import threading
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SYMBOL = "rp_native_running_process_wait_public"


def _required() -> bool:
    return bool(
        os.environ.get("GITHUB_ACTIONS")
        or os.environ.get("RUNNING_PROCESS_REQUIRE_NATIVE_DEBUGGER_SYMBOLS")
    )


def _worker() -> Path | None:
    explicit = os.environ.get("RUNNING_PROCESS_PROBE_WORKER")
    candidates = [Path(explicit)] if explicit else []
    machine = platform.machine().lower()
    arch = {
        "amd64": "x86_64",
        "x86_64": "x86_64",
        "arm64": "aarch64",
        "aarch64": "aarch64",
    }.get(machine)
    targeted = (
        [ROOT / "target" / f"{arch}-pc-windows-msvc" / "debug"]
        if arch is not None
        else []
    )
    candidates.extend(
        directory / "running-process-probe-worker.exe"
        for directory in [*targeted, ROOT / "target" / "debug"]
    )
    return next((path for path in candidates if path.is_file()), None)


def _llvm_tool(name: str) -> Path | None:
    candidates = [
        Path(r"C:\Program Files\LLVM\bin") / name,
        Path(r"C:\Program Files (x86)\LLVM\bin") / name,
    ]
    path_entry = next(
        (
            Path(directory) / name
            for directory in os.environ.get("PATH", "").split(os.pathsep)
            if directory and (Path(directory) / name).is_file()
        ),
        None,
    )
    if path_entry is not None:
        candidates.insert(0, path_entry)
    return next((path for path in candidates if path.is_file()), None)


def _export_rva(readobj: Path, image: Path, symbol: str) -> int:
    result = _run([str(readobj), "--coff-exports", str(image)])
    if result.returncode != 0:
        raise AssertionError(f"llvm-readobj failed: {result.stdout}\n{result.stderr}")
    for block in re.findall(r"Export\s*\{(.*?)\}", result.stdout, re.DOTALL):
        name = re.search(r"^\s*Name:\s*(\S+)\s*$", block, re.MULTILINE)
        rva = re.search(r"^\s*RVA:\s*(0x[0-9A-Fa-f]+|\d+)\s*$", block, re.MULTILINE)
        if name and rva and name.group(1) == symbol:
            return int(rva.group(1), 0)
    raise AssertionError(f"{symbol} was not exported by {image}")


def _codeview_identity(readobj: Path, image: Path) -> tuple[str, int, str]:
    result = _run([str(readobj), "--coff-debug-directory", str(image)])
    if result.returncode != 0:
        raise AssertionError(f"llvm-readobj failed: {result.stdout}\n{result.stderr}")
    guid = re.search(r"^\s*PDBGUID:\s*\{([^}]+)\}\s*$", result.stdout, re.MULTILINE)
    age = re.search(r"^\s*PDBAge:\s*(\d+)\s*$", result.stdout, re.MULTILINE)
    name = re.search(r"^\s*PDBFileName:\s*(.+?)\s*$", result.stdout, re.MULTILINE)
    if not (guid and age and name):
        raise AssertionError(
            f"no complete CodeView identity in {image}:\n{result.stdout}"
        )
    return guid.group(1), int(age.group(1)), Path(name.group(1)).name


def _pdb_identity(pdbutil: Path, pdb: Path) -> tuple[str, int]:
    result = _run([str(pdbutil), "dump", "-summary", str(pdb)])
    if result.returncode != 0:
        raise AssertionError(f"llvm-pdbutil failed: {result.stdout}\n{result.stderr}")
    guid = re.search(r"^\s*GUID:\s*\{([^}]+)\}\s*$", result.stdout, re.MULTILINE)
    age = re.search(r"^\s*Age:\s*(\d+)\s*$", result.stdout, re.MULTILINE)
    if not (guid and age):
        raise AssertionError(f"no PDB identity in {pdb}:\n{result.stdout}")
    return guid.group(1).upper(), int(age.group(1))


def _pdb_candidates(
    pdbutil: Path, pdb_name: str, expected_guid: str, expected_age: int
) -> list[Path]:
    machine = platform.machine().lower()
    arch = {
        "amd64": "x86_64",
        "x86_64": "x86_64",
        "arm64": "aarch64",
        "aarch64": "aarch64",
    }.get(machine)
    roots = [ROOT / "target" / "debug", ROOT / "target" / "release"]
    if arch is not None:
        target = ROOT / "target" / f"{arch}-pc-windows-msvc"
        preserved = ROOT / "target" / "probe-symbols" / f"{arch}-pc-windows-msvc"
        roots = [preserved, target / "debug", target / "release", *roots]
    candidates = {
        candidate.resolve()
        for root in roots
        for candidate in (root / pdb_name, root / "deps" / pdb_name)
        if candidate.is_file()
    }
    return sorted(
        (
            candidate
            for candidate in candidates
            if _pdb_identity(pdbutil, candidate) == (expected_guid, expected_age)
        ),
        key=str,
    )


def _run(
    command: list[str],
    *,
    input_text: str | None = None,
    env: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[str]:
    try:
        return subprocess.run(
            command,
            capture_output=True,
            text=True,
            check=False,
            encoding="utf-8",
            timeout=30,
            input=input_text,
            env=env,
        )
    except subprocess.TimeoutExpired as error:
        raise AssertionError(f"command timed out after 30s: {command}") from error


@unittest.skipUnless(sys.platform == "win32", "PE/PDB acceptance is Windows-specific")
class TestProbeInternalSymbolization(unittest.TestCase):
    def test_native_frame_resolves_beside_same_tid_python_frame(self) -> None:
        try:
            from running_process import _native
        except ImportError as error:
            if _required():
                self.fail(f"native extension is required: {error}")
            self.skipTest(f"native extension is not installed: {error}")
        image = Path(_native.__file__).resolve()
        worker = _worker()
        readobj = _llvm_tool("llvm-readobj.exe")
        pdbutil = _llvm_tool("llvm-pdbutil.exe")
        missing = [
            label
            for label, present in (
                ("running-process-probe-worker", worker is not None),
                ("llvm-readobj", readobj is not None),
                ("llvm-pdbutil", pdbutil is not None),
            )
            if not present
        ]
        if missing:
            message = "missing internal-symbolization prerequisites: " + ", ".join(
                missing
            )
            if _required():
                self.fail(message)
            self.skipTest(message)

        assert worker is not None
        assert readobj is not None
        assert pdbutil is not None
        guid, age, pdb_name = _codeview_identity(readobj, image)
        guid = guid.upper()
        pdb_candidates = _pdb_candidates(pdbutil, pdb_name, guid, age)
        if not pdb_candidates:
            message = (
                f"no build-tree {pdb_name} for installed image "
                f"CodeView GUID={guid} age={age}"
            )
            if _required():
                self.fail(message)
            self.skipTest(message)

        rva = _export_rva(readobj, image, SYMBOL)
        os_tid = threading.get_native_id()
        python_frame = {
            "file": "mixed_fixture.py",
            "line": 17,
            "func": "python_neighbor",
        }
        capture = {
            "format": "cooperative_frames",
            "modules": [
                {
                    "name": image.name,
                    "base_avma": 0,
                    "path_hint": str(image),
                }
            ],
            "threads": [
                {
                    "os_tid": os_tid,
                    "frames": [{"module_index": 0, "relative_address": rva}],
                    "py_frames": [python_frame],
                }
            ],
        }

        worker_env = os.environ.copy()
        worker_env["RUNNING_PROCESS_PROBE_SYMBOL_PATH"] = os.pathsep.join(
            str(path.parent) for path in pdb_candidates
        )
        result = _run(
            [str(worker)],
            input_text=json.dumps(capture),
            env=worker_env,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        thread = report["threads"][0]
        self.assertEqual(thread["os_tid"], os_tid)
        self.assertEqual(thread["py_frames"], [python_frame])
        self.assertIn(SYMBOL, thread["frames"][0]["function"])
        self.assertEqual(thread["frames"][0]["status"], "resolved")
        module = report["modules"][0]
        self.assertEqual(module["status"], "resolved")
        symbol_file = Path(module["symbol_file"]).resolve()
        self.assertIn(symbol_file, [path.resolve() for path in pdb_candidates])
        self.assertEqual(
            module["rejected_candidates"],
            0,
            f"worker rejected candidates for CodeView GUID={guid} age={age}",
        )


if __name__ == "__main__":
    unittest.main()
