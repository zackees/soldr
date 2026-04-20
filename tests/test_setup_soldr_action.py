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
