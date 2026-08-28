"""Unit contracts for the published-wheel Dylint smoke (soldr#2972)."""

from __future__ import annotations

import json
import os
import re
from pathlib import Path
from types import SimpleNamespace

import pytest
from conftest import (
    load_script_module,
    uv_pip_install_command,
    write_fake_soldr_console,
)

ROOT = Path(__file__).parents[1]
SCRIPTS = ROOT / ".github" / "scripts"
PUBLISHED_WORKFLOW = ROOT / ".github" / "workflows" / "published-dylint-smoke.yml"
RELEASE_WORKFLOW = ROOT / ".github" / "workflows" / "release-auto.yml"
PUBLISHED_SMOKE_TRIGGER_PATHS = (
    "Cargo.toml",
    "ci/smoke_published_dylint.py",
    "ci/canonical-targets.json",
    "dylints/**",
    "crates/soldr-cli/src/dylint_*.rs",
    "crates/soldr-cli/src/cli_args.rs",
    "crates/soldr-cli/src/soldr_main.rs",
    "crates/soldr-cli/src/soldr_main_dispatch.rs",
    "crates/soldr-cli/src/cargo_front_door/**",
    "crates/soldr-fetch/src/fetch/known_tools.rs",
    "crates/soldr-fetch/src/fetch/toolchain_packaged.rs",
    ".github/scripts/catalogue_http.py",
    ".github/scripts/check_dylint_driver_assets.py",
    ".github/scripts/toolchain_asset_query.py",
    ".github/workflows/published-dylint-smoke.yml",
    ".github/workflows/release-auto.yml",
    "tests/test_smoke_published_dylint.py",
)
load_script_module(SCRIPTS / "catalogue_http.py", "catalogue_http")
load_script_module(SCRIPTS / "toolchain_asset_query.py", "toolchain_asset_query")
load_script_module(
    SCRIPTS / "check_dylint_driver_assets.py", "check_dylint_driver_assets"
)
smoke = load_script_module(
    SCRIPTS.parent.parent / "ci" / "smoke_published_dylint.py", "smoke_published_dylint"
)


def test_selected_version_prefers_exact_pin_and_can_monitor_pypi_latest() -> None:
    assert smoke.selected_version("v0.9.11") == "0.9.11"

    assert (
        smoke.selected_version("", lambda _url: b'{"info":{"version":"0.9.12"}}')
        == "0.9.12"
    )


def test_isolated_environment_overrides_all_user_tool_homes(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    inherited_cargo = tmp_path / "inherited-cargo"
    monkeypatch.setenv("CARGO_HOME", str(inherited_cargo))
    monkeypatch.setenv(
        "PATH", os.pathsep.join([str(inherited_cargo / "bin"), "system-bin"])
    )
    for name in (
        "SOLDR_CACHE_DIR",
        "SOLDR_DYLINT_TOOLCHAIN",
        "SOLDR_DYLINT_CONFIGURED_TOOLCHAIN",
        "SOLDR_DYLINT_PREPARED_IDENTITY",
        "DYLINT_DRIVER_PATH",
        "RUSTUP_TOOLCHAIN",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "SOLDR_RUSTC_WRAPPER",
        "SOLDR_TOOLCHAIN_ORIGIN",
        "ZCCACHE_CACHE_DIR",
        "CARGO",
        "RUSTC",
    ):
        monkeypatch.setenv(name, f"ambient-{name.lower()}")
    monkeypatch.setenv("SOLDR_GITHUB_TOKEN", "auth-is-not-routing")
    env = smoke.isolated_environment(tmp_path)
    assert env["HOME"] == str(tmp_path / "home")
    assert env["USERPROFILE"] == str(tmp_path / "home")
    assert env["CARGO_HOME"] == str(tmp_path / "cargo-home")
    assert env["RUSTUP_HOME"] == str(tmp_path / "rustup-home")
    assert env["SOLDR_CACHE_DIR"] == str(tmp_path / "soldr-cache")
    assert env["UV_CACHE_DIR"] == str(tmp_path / "uv-cache")
    assert env["SOLDR_GITHUB_TOKEN"] == "auth-is-not-routing"
    assert str(inherited_cargo / "bin") not in env["PATH"]
    assert "system-bin" in env["PATH"]
    for name in (
        "SOLDR_DYLINT_TOOLCHAIN",
        "SOLDR_DYLINT_CONFIGURED_TOOLCHAIN",
        "SOLDR_DYLINT_PREPARED_IDENTITY",
        "DYLINT_DRIVER_PATH",
        "RUSTUP_TOOLCHAIN",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "SOLDR_RUSTC_WRAPPER",
        "SOLDR_TOOLCHAIN_ORIGIN",
        "ZCCACHE_CACHE_DIR",
        "CARGO",
        "RUSTC",
    ):
        assert name not in env


def test_smoke_installs_exact_wheel_and_proves_version_channel_and_driver(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    repo = tmp_path / "repo"
    manifest = repo / "dylints" / "one" / "rust-toolchain.toml"
    manifest.parent.mkdir(parents=True)
    manifest.write_text(
        '[toolchain]\nchannel = "nightly-2026-05-28"\n', encoding="utf-8"
    )
    venv = tmp_path / "venv"
    state = tmp_path / "state"
    calls: list[list[str]] = []
    soldr = venv / "Scripts" / "soldr.exe"
    probe = smoke.driver_probe_command(
        soldr,
        repo_root=repo,
        manifest=state / "driver-probe" / "Cargo.toml",
    )

    def fake_run(command: list[str], **_kwargs: object) -> SimpleNamespace:
        calls.append(command)
        if "--list" in command:
            raise AssertionError("published cargo-dylint 6 rejects deprecated --list")
        if command == ["uv", "venv", "--clear", str(venv)]:
            return SimpleNamespace(returncode=0, stdout="", stderr="")
        if command == uv_pip_install_command(
            venv, "--only-binary=:all:", "soldr==0.9.11"
        ):
            write_fake_soldr_console(venv, windows=True)
            return SimpleNamespace(returncode=0, stdout="", stderr="")
        if command == [str(soldr), "version", "--json"]:
            return SimpleNamespace(
                returncode=0, stdout=json.dumps({"soldr_version": "0.9.11"}), stderr=""
            )
        if command == [str(soldr), "dylint", "prepare"]:
            return SimpleNamespace(
                returncode=0, stdout="", stderr="channel nightly-2026-05-28"
            )
        if command == probe:
            return SimpleNamespace(returncode=0, stdout="", stderr="")
        raise AssertionError(f"unexpected published-Dylint command: {command}")

    monkeypatch.setattr(smoke.subprocess, "run", fake_run)
    smoke.smoke(version="0.9.11", repo_root=repo, venv=venv, state_root=state)
    assert calls[0] == ["uv", "venv", "--clear", str(venv)]
    assert calls[1] == uv_pip_install_command(
        venv, "--only-binary=:all:", "soldr==0.9.11"
    )
    assert calls[-1] == [
        str(soldr),
        "dylint",
        "--manifest-path",
        str(state / "driver-probe" / "Cargo.toml"),
        "--path",
        str(repo / "dylints"),
        "--pattern",
        "ban_raw_env_flag",
    ]


def test_driver_probe_rejects_removed_list_flag_and_targets_only_one_lint(
    tmp_path: Path,
) -> None:
    command = smoke.driver_probe_command(
        tmp_path / "soldr.exe",
        repo_root=tmp_path / "repo",
        manifest=tmp_path / "probe" / "Cargo.toml",
    )

    assert "--list" not in command
    assert command[:2] == [str(tmp_path / "soldr.exe"), "dylint"]
    assert command[2:] == [
        "--manifest-path",
        str(tmp_path / "probe" / "Cargo.toml"),
        "--path",
        str(tmp_path / "repo" / "dylints"),
        "--pattern",
        "ban_raw_env_flag",
    ]


def test_smoke_rejects_wrong_binary_version_or_channel(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    repo = tmp_path / "repo"
    manifest = repo / "dylints" / "one" / "rust-toolchain.toml"
    manifest.parent.mkdir(parents=True)
    manifest.write_text(
        '[toolchain]\nchannel = "nightly-2026-05-28"\n', encoding="utf-8"
    )
    venv = tmp_path / "venv"

    def fake_run(command, **_kwargs):
        if command[:2] == ["uv", "pip"]:
            binary = venv / "Scripts" / "soldr.exe"
            binary.parent.mkdir(parents=True)
            binary.write_bytes(b"")
        if command[-2:] == ["version", "--json"]:
            return SimpleNamespace(
                returncode=0, stdout='{"soldr_version":"0.0.1"}', stderr=""
            )
        return SimpleNamespace(returncode=0, stdout="", stderr="")

    monkeypatch.setattr(smoke.subprocess, "run", fake_run)
    with pytest.raises(smoke.PublishedDylintSmokeError, match="provenance mismatch"):
        smoke.smoke(
            version="0.9.11", repo_root=repo, venv=venv, state_root=tmp_path / "state"
        )


def test_windows_monitor_and_release_gate_orchestrate_the_tested_script() -> None:
    monitor = PUBLISHED_WORKFLOW.read_text(encoding="utf-8")
    release = RELEASE_WORKFLOW.read_text(encoding="utf-8")
    for trigger in (
        "branches: [main]",
        "pull_request:",
        "schedule:",
        "workflow_dispatch:",
    ):
        assert trigger in monitor
    for trigger in ("push", "pull_request"):
        match = re.search(
            rf"^  {trigger}:\n(?:    branches: \[main\]\n)?    paths:\n(?P<paths>(?:      - '[^']+'\n)+)",
            monitor,
            flags=re.MULTILINE,
        )
        assert match is not None, f"{trigger} must path-filter the published smoke"
        assert tuple(re.findall(r"'([^']+)'", match.group("paths"))) == (
            PUBLISHED_SMOKE_TRIGGER_PATHS
        )
    assert "runs-on: windows-2025" in monitor
    assert "ci/smoke_published_dylint.py" in monitor
    assert "python-version: '3.13'" in monitor
    assert "astral-sh/setup-uv@" in monitor
    assert "smoke-published-dylint:" in release
    assert (
        "needs.publish-pypi.result == 'success' || needs.publish-pypi.result == 'skipped'"
        in release
    )
    assert (
        'ci/smoke_published_dylint.py --expected-version "${{ needs.prepare.outputs.version }}"'
        in release
    )
    assert "- smoke-published-dylint" in release
    assert "needs.smoke-published-dylint.result != 'success'" in release
