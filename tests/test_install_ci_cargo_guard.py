from __future__ import annotations

import os
import subprocess
from pathlib import Path

import pytest
from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / ".github" / "scripts" / "install_ci_cargo_guard.py"
guard = load_script_module(SCRIPT)


@pytest.mark.skipif(os.name == "nt", reason="executes the POSIX shim contract")
def test_allowed_cargo_reenters_source_soldr_while_bare_cargo_fails_closed(
    tmp_path: Path,
) -> None:
    source_soldr = tmp_path / "source soldr"
    invocation_log = tmp_path / "invocation.log"
    source_soldr.write_text(
        "#!/bin/sh\n"
        f"printf '%s\\n' \"$@\" > {guard.shlex.quote(str(invocation_log))}\n",
        encoding="utf-8",
    )
    source_soldr.chmod(0o755)
    real_cargo = tmp_path / "rustup" / "toolchains" / "stable" / "bin" / "cargo"
    real_cargo.parent.mkdir(parents=True)
    real_cargo.write_text("real cargo placeholder", encoding="utf-8")

    paths = guard.install_guard(
        source_soldr=source_soldr,
        real_cargo=real_cargo,
        output_dir=tmp_path / "guard",
        platform="posix",
    )
    environment = os.environ.copy()
    environment.update(
        {
            "CARGO": str(paths.allowed_cargo),
            "SOLDR_REAL_CARGO": str(real_cargo),
            "PATH": f"{paths.trap_dir}{os.pathsep}{environment['PATH']}",
        }
    )

    allowed = subprocess.run(
        [environment["CARGO"], "metadata", "--no-deps"],
        env=environment,
        text=True,
        capture_output=True,
        check=False,
    )
    assert allowed.returncode == 0
    assert invocation_log.read_text(encoding="utf-8").splitlines() == [
        "cargo",
        "metadata",
        "--no-deps",
    ]

    trapped = subprocess.run(
        ["cargo", "metadata"],
        env=environment,
        text=True,
        capture_output=True,
        check=False,
    )
    assert trapped.returncode == guard.TRAP_EXIT_CODE
    assert "unexpected bare cargo invocation" in trapped.stderr
    assert "$CARGO" in trapped.stderr


@pytest.mark.skipif(os.name == "nt", reason="executes the POSIX shim contract")
def test_nextest_runner_restores_cargo_after_cargo_overwrites_it(
    tmp_path: Path,
) -> None:
    source_soldr = tmp_path / "soldr"
    source_soldr.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
    source_soldr.chmod(0o755)
    real_cargo = tmp_path / "rustup" / "cargo"
    real_cargo.parent.mkdir()
    real_cargo.write_bytes(b"")
    paths = guard.install_guard(
        source_soldr=source_soldr,
        real_cargo=real_cargo,
        output_dir=tmp_path / "guard",
        platform="posix",
    )
    probe = tmp_path / "test-process"
    probe.write_text("#!/bin/sh\nprintf '%s\\n' \"$CARGO\"\n", encoding="utf-8")
    probe.chmod(0o755)
    cargo_overwritten = os.environ.copy()
    # Cargo documents that it replaces an inherited CARGO value with the
    # binary performing the build before crates and test tooling run.
    cargo_overwritten["CARGO"] = str(real_cargo)

    without_runtime_boundary = subprocess.run(
        [probe],
        env=cargo_overwritten,
        text=True,
        capture_output=True,
        check=False,
    )
    assert without_runtime_boundary.stdout.strip() == str(real_cargo)

    through_nextest_runner = subprocess.run(
        [paths.test_runner, probe],
        env=cargo_overwritten,
        text=True,
        capture_output=True,
        check=False,
    )
    assert through_nextest_runner.returncode == 0
    assert through_nextest_runner.stdout.strip() == str(paths.allowed_cargo)


def test_windows_shims_keep_allowed_and_trapped_names_separate(tmp_path: Path) -> None:
    source_soldr = tmp_path / "source soldr.exe"
    source_soldr.write_bytes(b"")
    real_cargo = tmp_path / "rustup" / "cargo.exe"
    real_cargo.parent.mkdir()
    real_cargo.write_bytes(b"")

    paths = guard.install_guard(
        source_soldr=source_soldr,
        real_cargo=real_cargo,
        output_dir=tmp_path / "guard",
        platform="windows",
    )

    assert paths.allowed_cargo.name == "soldr-ci-cargo.cmd"
    assert paths.allowed_cargo.parent.name == "allowed"
    assert paths.trap_dir.joinpath("cargo.cmd").is_file()
    allowed = paths.allowed_cargo.read_text(encoding="utf-8")
    trapped = paths.trap_dir.joinpath("cargo.cmd").read_text(encoding="utf-8")
    runner = paths.test_runner.read_text(encoding="utf-8")
    assert f'"{source_soldr}" cargo %*' in allowed
    assert "unexpected bare cargo invocation" in trapped
    assert f"exit /b {guard.TRAP_EXIT_CODE}" in trapped
    assert f'set "CARGO={paths.allowed_cargo}"' in runner
    assert "call %*" in runner
    assert (tmp_path / "guard" / "allowed-cargo-path").read_text(
        encoding="utf-8"
    ).strip() == str(paths.allowed_cargo)
    assert (tmp_path / "guard" / "test-runner-path").read_text(
        encoding="utf-8"
    ).strip() == str(paths.test_runner)


@pytest.mark.parametrize("argument", ("source_soldr", "real_cargo", "output_dir"))
def test_guard_rejects_relative_boundary_paths(tmp_path: Path, argument: str) -> None:
    source_soldr = tmp_path / "soldr"
    source_soldr.write_bytes(b"")
    real_cargo = tmp_path / "cargo"
    real_cargo.write_bytes(b"")
    values = {
        "source_soldr": source_soldr,
        "real_cargo": real_cargo,
        "output_dir": tmp_path / "guard",
        "platform": "posix",
    }
    values[argument] = Path("relative")

    with pytest.raises(ValueError, match="absolute path"):
        guard.install_guard(**values)
