from __future__ import annotations

import importlib.util
import json
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = REPO_ROOT / ".github" / "actions" / "setup-soldr" / "resolve_setup.py"


def _load_module():
    spec = importlib.util.spec_from_file_location("resolve_setup", SCRIPT_PATH)
    module = importlib.util.module_from_spec(spec)
    assert spec is not None
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


def test_load_toolchain_spec_reads_rust_toolchain_toml() -> None:
    module = _load_module()

    spec = module.load_toolchain_spec(REPO_ROOT, "rust-toolchain.toml", "")

    assert spec["channel"] == "1.94.1"
    assert spec["profile"] == "minimal"
    assert "rustfmt" in spec["components"]
    assert "clippy" in spec["components"]
    assert spec["source"] == "rust-toolchain.toml"
    assert spec["file_hash"] != "none"


def test_load_toolchain_spec_defaults_when_file_missing(tmp_path: Path) -> None:
    module = _load_module()

    spec = module.load_toolchain_spec(tmp_path, "missing.toml", "")

    assert spec == {
        "channel": "stable",
        "profile": "minimal",
        "components": [],
        "targets": [],
        "source": "default",
        "file_hash": "none",
    }


def test_load_toolchain_spec_prefers_explicit_override() -> None:
    module = _load_module()

    spec = module.load_toolchain_spec(REPO_ROOT, "rust-toolchain.toml", "stable")

    assert spec["channel"] == "stable"
    assert spec["profile"] == "minimal"
    assert spec["source"] == "input"


def test_normalize_list_accepts_csv_strings() -> None:
    module = _load_module()

    value = module._normalize_list("rustfmt, clippy, ")

    assert value == ["rustfmt", "clippy"]


def test_toolchain_signature_payload_is_json_serializable() -> None:
    module = _load_module()

    spec = module.load_toolchain_spec(REPO_ROOT, "rust-toolchain.toml", "")

    payload = {
        "channel": spec["channel"],
        "profile": spec["profile"],
        "components": spec["components"],
        "targets": spec["targets"],
        "source": spec["source"],
        "file_hash": spec["file_hash"],
        "soldr_repo": "zackees/soldr",
        "soldr_version": "0.7.4",
    }

    assert json.loads(json.dumps(payload))["channel"] == "1.94.1"


def test_main_creates_cache_layout_and_outputs(tmp_path: Path, monkeypatch) -> None:
    module = _load_module()
    workspace = tmp_path / "workspace"
    runner_temp = tmp_path / "runner-temp"
    workspace.mkdir()
    runner_temp.mkdir()

    github_env = tmp_path / "github.env"
    github_output = tmp_path / "github.output"
    github_path = tmp_path / "github.path"

    monkeypatch.setenv("ACTION_WORKSPACE", str(workspace))
    monkeypatch.setenv("RUNNER_TEMP", str(runner_temp))
    monkeypatch.setenv("ACTION_OS", "Linux")
    monkeypatch.setenv("ACTION_ARCH", "X64")
    monkeypatch.setenv("INPUT_REPO", "zackees/soldr")
    monkeypatch.setenv("INPUT_VERSION", "")
    monkeypatch.setenv("INPUT_CACHE_DIR", "")
    monkeypatch.setenv("INPUT_CACHE_KEY_SUFFIX", "")
    monkeypatch.setenv("INPUT_TOOLCHAIN", "")
    monkeypatch.setenv("INPUT_TOOLCHAIN_FILE", "missing.toml")
    monkeypatch.setenv("INPUT_TRUST_MODE", "")
    monkeypatch.setenv("INPUT_TARGET_DIR", "custom-target")
    monkeypatch.setenv("GITHUB_SHA", "abc123")
    monkeypatch.setenv("GITHUB_ENV", str(github_env))
    monkeypatch.setenv("GITHUB_OUTPUT", str(github_output))
    monkeypatch.setenv("GITHUB_PATH", str(github_path))

    module.main()

    cache_root = runner_temp / "setup-soldr"
    assert cache_root.is_dir()
    assert (cache_root / "soldr").is_dir()
    assert (cache_root / "soldr" / "cache").is_dir()
    assert (cache_root / "soldr" / "bin").is_dir()
    assert (cache_root / "cargo").is_dir()
    assert (cache_root / "cargo" / "bin").is_dir()
    assert (cache_root / "rustup").is_dir()
    assert (cache_root / "bin").is_dir()

    outputs = github_output.read_text(encoding="utf-8")
    assert f"cache_root={cache_root}" in outputs
    assert "cache_key=setup-soldr-v0-linux-x64-" in outputs
    assert "cache_restore_prefix=setup-soldr-v0-linux-x64-" in outputs
    assert "build_cache_key=setup-soldr-buildcache-v0-linux-x64-" in outputs
    assert "build_cache_restore_key_toolchain=setup-soldr-buildcache-v0-linux-x64-" in outputs
    assert "build_cache_restore_key_os_arch=setup-soldr-buildcache-v0-linux-x64-" in outputs
    assert f"target_cache_path={workspace / 'custom-target'}" in outputs
    assert "target_cache_key=setup-soldr-targetcache-v0-linux-x64-" in outputs
    assert outputs.count("-no-lock-abc123") == 1
    assert "toolchain=stable" in outputs
