"""The target-run lane must pin RUSTUP_HOME, not just the channel (soldr#3195).

Fixtures that isolate `HOME` otherwise send rustup looking for the provisioned
toolchain under an empty temp home. Before the Nextest wrapper refused
test-time downloads, rustup silently reinstalled it inside each such test;
after, those tests failed. These cover the reader and the wiring.
"""

from __future__ import annotations

from pathlib import Path

import pytest
from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / ".github" / "scripts" / "rustup_home_from_toolchain_binary.py"
TARGET_RUN = REPO_ROOT / ".github" / "workflows" / "_ci-target-run.yml"

reader = load_script_module(SCRIPT, "rustup_home_from_toolchain_binary")


@pytest.mark.parametrize(
    ("binary", "home"),
    [
        (
            "/home/runner/.rustup/toolchains/1.95.0-aarch64-unknown-linux-gnu/bin/cargo",
            "/home/runner/.rustup",
        ),
        (
            "/root/.soldr/rustup/toolchains/1.95.0-x86_64-unknown-linux-musl/bin/rustc",
            "/root/.soldr/rustup",
        ),
        (
            r"C:\Users\runner\.rustup\toolchains\1.95.0-x86_64-pc-windows-msvc\bin\cargo.exe",
            r"C:\Users\runner\.rustup",
        ),
    ],
)
def test_the_home_is_read_from_a_provisioned_binary(binary: str, home: str) -> None:
    assert reader.rustup_home_from(binary) == home


@pytest.mark.parametrize(
    "binary",
    [
        "/home/runner/.cargo/bin/cargo",
        "/usr/bin/cargo",
        "cargo",
        "/home/runner/.rustup/toolchains/bin/cargo",
        "/home/runner/.rustup/somewhere/1.95.0/bin/cargo",
        "",
    ],
)
def test_any_other_shape_is_refused_rather_than_guessed(binary: str) -> None:
    assert reader.rustup_home_from(binary) is None


def test_the_cli_prints_the_home_and_refuses_bad_input(
    capsys: pytest.CaptureFixture[str],
) -> None:
    good = "/home/runner/.rustup/toolchains/1.95.0-aarch64-unknown-linux-gnu/bin/cargo"
    assert reader.main([good]) == 0
    assert capsys.readouterr().out.strip() == "/home/runner/.rustup"
    assert reader.main(["/usr/bin/cargo"]) == 1


def test_the_target_run_lane_exports_rustup_home_from_the_provisioned_cargo() -> None:
    text = TARGET_RUN.read_text(encoding="utf-8")
    assert (
        'rustup_home=$(python .github/scripts/rustup_home_from_toolchain_binary.py "$cargo_bin")'
        in text
    )
    assert 'echo "RUSTUP_HOME=$rustup_home"' in text
