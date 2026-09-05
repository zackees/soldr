from __future__ import annotations

import os
from pathlib import Path

from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / ".github" / "scripts" / "install_soldr_maturin.py"
MODULE = load_script_module(SCRIPT, "install_soldr_maturin")


def test_distribution_comes_from_runtime_contract() -> None:
    assert MODULE.distribution() == ("soldr-maturin", "1.14.1.post1")


def test_install_command_forces_an_uncached_sdist_build() -> None:
    assert MODULE.install_command(Path("python")) == [
        "python",
        "-m",
        "pip",
        "install",
        "--no-cache-dir",
        "--no-binary",
        "soldr-maturin",
        "soldr-maturin==1.14.1.post1",
    ]


def test_source_build_environment_uses_real_rustc_and_soldr_cargo() -> None:
    rustc = Path("toolchains") / "bin" / "rustc"
    cargo = Path("shims") / "cargo"
    env = MODULE.source_build_environment(
        {
            "KEEP": "yes",
            "PATH": "caller-bin",
            "CARGO_BUILD_TARGET": "x86_64-pc-windows-msvc",
            "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER": "lld-link",
            "MATURIN_USE_XWIN": "0",
            "RUSTC_WRAPPER": "stale-wrapper",
        },
        rustc=rustc,
        cargo=cargo,
    )
    resolved_rustc = rustc.resolve()
    resolved_cargo = cargo.resolve()
    assert env == {
        "KEEP": "yes",
        "PATH": (
            f"{resolved_rustc.parent}{os.pathsep}{resolved_cargo.parent}{os.pathsep}caller-bin"
        ),
        "CARGO": str(resolved_cargo),
        "RUSTC": str(resolved_rustc),
        "RUSTUP_TOOLCHAIN": "1.95.0",
    }


def test_ci_invokes_the_tested_installer() -> None:
    workflow = (REPO_ROOT / ".github" / "workflows" / "ci.yml").read_text(
        encoding="utf-8"
    )
    assert "python .github/scripts/install_soldr_maturin.py" in workflow
    assert "pip install --no-cache-dir --no-binary soldr-maturin" not in workflow
