"""Regression locks for checkout-built Soldr isolation in CI smokes."""

from pathlib import Path
from unittest import mock

from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPTS = REPO_ROOT / ".github" / "scripts"


def _outer_env() -> dict[str, str]:
    return {
        "PATH": "/tools",
        "RUSTC_WRAPPER": "/setup-soldr/shims/rustc",
        "RUSTC_WORKSPACE_WRAPPER": "/setup-soldr/shims/workspace-rustc",
        "SOLDR_BROKER_PROGRAM": "old-broker",
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
    assert module.fresh_checkout_env(source, broker_program="fresh-broker") == {
        "PATH": "/tools",
        "SOLDR_BROKER_PROGRAM": "fresh-broker",
    }
    assert source["SOLDR_BROKER_SERVICE"] == "old-route"
    env = {"SOLDR_BROKER_PROGRAM": "fresh-broker"}
    with mock.patch.object(module.subprocess, "run") as run:
        module.stop_soldr_broker("soldr", env)
    run.assert_called_once_with(
        ["soldr", "broker", "stop", "--program", "fresh-broker"],
        env=env,
        timeout=20,
        check=False,
    )


def test_pep517_smoke_uses_wheel_owned_wrapper_and_daemon() -> None:
    module = load_script_module(
        SCRIPTS / "pep517_daemon_smoke.py", "pep517_daemon_smoke_env"
    )
    source = _outer_env()
    _assert_isolated(module.isolated_smoke_env(source))
    assert module.isolated_smoke_env(source, broker_program="fresh-broker") == {
        "PATH": "/tools",
        "SOLDR_BROKER_PROGRAM": "fresh-broker",
    }
    assert source["SOLDR_BROKER_SERVICE"] == "old-route"
    env = {"SOLDR_BROKER_PROGRAM": "fresh-broker"}
    with mock.patch.object(module.subprocess, "run") as run:
        module.stop_soldr_broker(Path("soldr"), env)
    run.assert_called_once_with(
        ["soldr", "broker", "stop", "--program", "fresh-broker"],
        env=env,
        timeout=20,
        check=False,
    )
