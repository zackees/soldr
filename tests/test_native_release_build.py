from __future__ import annotations

import importlib.util
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / ".github" / "scripts" / "native_release_build.py"


def load_module():
    spec = importlib.util.spec_from_file_location("native_release_build", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


MODULE = load_module()


def test_musl_release_source_builds_the_pinned_soldr_maturin() -> None:
    assert MODULE.SOLDR_MATURIN_REQUIREMENT == "soldr-maturin==1.14.1.post1"
    assert MODULE.SOLDR_MATURIN_NO_BINARY == "soldr-maturin"


def test_cargo_command_routes_through_pinned_soldr_rustup() -> None:
    driver = Path("release-tools") / "soldr"
    command = MODULE.cargo_command(driver, "build", "--release")
    assert command == [
        str(driver),
        "rustup",
        "run",
        "1.95.0",
        "cargo",
        "build",
        "--release",
    ]


def test_soldr_cli_version_requires_exactly_one_package() -> None:
    assert (
        MODULE.soldr_cli_version(
            '{"packages":[{"name":"soldr-cli","version":"0.9.0"}]}'
        )
        == "0.9.0"
    )
    with pytest.raises(RuntimeError, match="expected one soldr-cli package"):
        MODULE.soldr_cli_version('{"packages":[]}')


def test_release_build_environment_is_bounded_and_reproducible() -> None:
    env = MODULE.release_build_environment(
        {"KEEP": "yes", "SOLDR_JOBS": "99", "RUSTC_WRAPPER": "stale-wrapper"}
    )
    assert env["KEEP"] == "yes"
    assert env["SOLDR_RELEASE_CI"] == "1"
    assert env["CARGO_PROFILE_RELEASE_LTO"] == "thin"
    assert env["CARGO_PROFILE_RELEASE_CODEGEN_UNITS"] == "1"
    assert env["CARGO_BUILD_JOBS"] == "2"
    assert env["SOLDR_JOBS"] == "2"
    assert "RUSTC_WRAPPER" not in env


def test_wheel_environment_points_maturin_at_soldr_cargo_bridge() -> None:
    driver = Path("release-tools") / "soldr"
    cargo_bridge = Path("release-tools") / "cargo-via-soldr"
    env = MODULE.wheel_environment(
        {"KEEP": "yes", "RUSTC_WRAPPER": "stale-wrapper"},
        driver=driver,
        cargo_bridge=cargo_bridge,
    )
    assert env == {
        "KEEP": "yes",
        "SOLDR_RELEASE_DRIVER": str(driver),
        "SOLDR_RELEASE_TOOLCHAIN": "1.95.0",
        "CARGO": str(cargo_bridge),
    }


def test_host_tool_environment_strips_musl_cross_state() -> None:
    driver = Path("release-tools") / "soldr"
    cargo_bridge = Path("release-tools") / "cargo-via-soldr"
    rustc = Path("toolchains") / "1.95.0" / "bin" / "rustc"
    env = MODULE.host_tool_environment(
        {
            "KEEP": "yes",
            "PATH": "caller-bin",
            "CARGO_BUILD_TARGET": "aarch64-unknown-linux-musl",
            "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER": "musl-gcc",
            "CC_aarch64_unknown_linux_musl": "musl-gcc",
            "RUSTC_WRAPPER": "stale-wrapper",
        },
        driver=driver,
        cargo_bridge=cargo_bridge,
        rustc=rustc,
    )
    resolved_rustc = rustc.resolve()
    assert env == {
        "KEEP": "yes",
        "PATH": f"{resolved_rustc.parent}{MODULE.os.pathsep}caller-bin",
        "SOLDR_RELEASE_DRIVER": str(driver),
        "SOLDR_RELEASE_TOOLCHAIN": "1.95.0",
        "CARGO": str(cargo_bridge),
        "RUSTC": str(resolved_rustc),
        "RUSTUP_TOOLCHAIN": "1.95.0",
        "CARGO_BUILD_JOBS": "2",
        "SOLDR_JOBS": "2",
    }
