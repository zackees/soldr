#!/usr/bin/env python3
"""Verify that packed Darwin DWARF survives inside the Mach-O executable.

This is intentionally a host-only check: the binary is never executed.  It
cross-compiles a temporary crate through ``soldr`` and then removes every
external dSYM/object sidecar before asking LLVM to read the executable's own
DWARF sections.

Examples::

    uv run --no-project python bench/test_darwin_symbols.py
    DARWIN_TARGETS=x86_64-apple-darwin,aarch64-apple-darwin \
        uv run --no-project python bench/test_darwin_symbols.py

Set ``LLVM_DWARFDUMP`` (or ``SOLDR_LLVM_DIR``) when llvm-dwarfdump is not on
PATH.  The temporary project and all build output are deleted automatically.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

DEFAULT_TARGET = "x86_64-apple-darwin"


def tool(name: str) -> str:
    override = os.environ.get("LLVM_DWARFDUMP") if name == "llvm-dwarfdump" else None
    candidates = [override] if override else []
    llvm_dir = os.environ.get("SOLDR_LLVM_DIR")
    if llvm_dir:
        candidates.append(str(Path(llvm_dir) / (name + (".exe" if os.name == "nt" else ""))))
    managed_bin = Path.home() / ".soldr" / "bin"
    executable = name + (".exe" if os.name == "nt" else "")
    candidates.extend(
        str(path)
        for pattern in (
            f"llvm-*/bin/{executable}",
            f"syslib/llvm-tools/*/*/package/bin/{executable}",
        )
        for path in managed_bin.glob(pattern)
    )
    found = shutil.which(name)
    if found:
        candidates.append(found)
    for candidate in candidates:
        if candidate and Path(candidate).is_file():
            return candidate
    raise RuntimeError(
        f"{name} is required; install LLVM or set LLVM_DWARFDUMP/SOLDR_LLVM_DIR"
    )


def run(command: list[str], *, cwd: Path) -> str:
    print("+", " ".join(command))
    result = subprocess.run(command, cwd=cwd, text=True, capture_output=True)
    if result.returncode:
        sys.stderr.write(result.stdout)
        sys.stderr.write(result.stderr)
        raise RuntimeError(f"command failed ({result.returncode}): {command[0]}")
    return result.stdout + result.stderr


def check_target(target: str) -> None:
    with tempfile.TemporaryDirectory(prefix="soldr-darwin-symbols-") as raw:
        project = Path(raw)
        (project / "Cargo.toml").write_text(
            """[package]\nname = \"darwin-symbol-probe\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[profile.release]\ndebug = \"line-tables-only\"\nsplit-debuginfo = \"packed\"\n""",
            encoding="utf-8",
        )
        src = project / "src"
        src.mkdir()
        (src / "main.rs").write_text(
            """#[inline(never)]\n+fn symbol_probe(value: u64) -> u64 {\n    value.wrapping_mul(37).wrapping_add(11)\n}\n\nfn main() {\n    println!(\"{}\", symbol_probe(7));\n}\n""",
            encoding="utf-8",
        )

        run(["soldr", "build", "--release", "--target", target], cwd=project)
        binary = project / "target" / target / "release" / "darwin-symbol-probe"
        if not binary.is_file():
            raise RuntimeError(f"cross-build did not produce {binary}")

        # The test is specifically executable-only.  Remove all sidecars and
        # object files before inspecting the Mach-O.
        for path in project.rglob("*"):
            if path.is_dir() and path.name.endswith(".dSYM"):
                shutil.rmtree(path)
            elif path.is_file() and path.suffix in {".o", ".dwo"}:
                path.unlink()

        dump = run(
            [tool("llvm-dwarfdump"), "--debug-info", "--debug-line", str(binary)],
            cwd=project,
        )
        if "symbol_probe" not in dump or "main.rs" not in dump:
            raise RuntimeError(
                f"{target}: embedded DWARF did not identify symbol_probe/main.rs"
            )
        print(f"PASS {target}: embedded DWARF resolves symbol_probe in {binary}")


def main() -> int:
    targets = [
        value.strip()
        for value in os.environ.get("DARWIN_TARGETS", DEFAULT_TARGET).split(",")
        if value.strip()
    ]
    for target in targets:
        if not target.endswith("-apple-darwin"):
            raise SystemExit(f"invalid Darwin target: {target}")
        check_target(target)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, RuntimeError) as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1)
