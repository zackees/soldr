"""Regression locks for checkout-built Soldr isolation in CI smokes."""

from pathlib import Path

from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPTS = REPO_ROOT / ".github" / "scripts"


def _outer_env() -> dict[str, str]:
    return {
        "PATH": "/tools",
        "RUSTC_WRAPPER": "/setup-soldr/shims/rustc",
        "RUSTC_WORKSPACE_WRAPPER": "/setup-soldr/shims/workspace-rustc",
        "SOLDR_BROKER_SERVICE": "old-route",
        "SOLDR_INTERNAL_DAEMON_EXE": "/setup-soldr/soldr-daemon",
    }


def _assert_isolated(env: dict[str, str]) -> None:
    assert env == {"PATH": "/tools"}


def test_gnu_proof_uses_checkout_owned_wrapper_and_daemon() -> None:
    module = load_script_module(
        SCRIPTS / "gnu_linux_toolchain_e2e.py", "gnu_linux_toolchain_e2e_env"
    )
    source = _outer_env()
    _assert_isolated(module.fresh_checkout_env(source))
    assert source["SOLDR_BROKER_SERVICE"] == "old-route"


def test_pep517_smoke_uses_wheel_owned_wrapper_and_daemon() -> None:
    module = load_script_module(
        SCRIPTS / "pep517_daemon_smoke.py", "pep517_daemon_smoke_env"
    )
    source = _outer_env()
    _assert_isolated(module.isolated_smoke_env(source))
    assert source["SOLDR_BROKER_SERVICE"] == "old-route"
