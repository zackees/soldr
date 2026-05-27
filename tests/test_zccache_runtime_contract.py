from __future__ import annotations

import hashlib
import importlib.util
import json
from pathlib import Path

import pytest


REPO_ROOT = Path(__file__).resolve().parents[1]
CONTRACT_PATH = REPO_ROOT / "contracts" / "zccache-runtime.v1.json"
PY_CONTRACT_PATH = REPO_ROOT / ".github" / "actions" / "setup-soldr" / "zccache_contract.py"


def _load_py_contract():
    spec = importlib.util.spec_from_file_location("zccache_contract", PY_CONTRACT_PATH)
    module = importlib.util.module_from_spec(spec)
    assert spec is not None
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


def _sha256(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def _write_manifest_fixture(root: Path, *, windows: bool = False) -> dict[str, object]:
    module = _load_py_contract()
    names = module.release_binary_names(windows=windows)
    payloads: dict[str, bytes] = {}
    for name in names:
        payload = f"{name}\n".encode("utf-8")
        payloads[name] = payload
        (root / name).write_bytes(payload)

    suffix = ".exe" if windows else ""
    return {
        "schema_version": 2,
        "soldr": {
            "version": "0.7.39",
            "target": "x86_64-unknown-linux-gnu",
            "binary": f"soldr{suffix}",
            "sha256": _sha256(payloads[f"soldr{suffix}"]),
            "commit_sha": "abc123",
        },
        "zccache": {
            "version": module.CONTRACT["zccache"]["managed_version"],
            "target": "x86_64-unknown-linux-musl",
            "binaries": [
                {
                    "name": f"{base}{suffix}",
                    "sha256": _sha256(payloads[f"{base}{suffix}"]),
                }
                for base in module.ZCCACHE_BUNDLED_BINARIES
            ],
        },
        "crgx": {
            "version": module.CONTRACT["crgx"]["managed_version"],
            "target": "x86_64-unknown-linux-gnu",
            "binary": f"crgx{suffix}",
            "sha256": _sha256(payloads[f"crgx{suffix}"]),
            "source_commit": "def456",
        },
        "archive": {
            "format": module.ARCHIVE_EXT,
            "compression_level": module.CONTRACT["release_archive"]["compression_level"],
        },
        "built_at": "2026-05-27T00:00:00Z",
    }


def test_contract_json_has_expected_shape() -> None:
    contract = json.loads(CONTRACT_PATH.read_text(encoding="utf-8"))

    assert contract["schema_version"] == 1
    assert contract["release_archive"]["extension"] == "tar.zst"
    assert contract["release_archive"]["manifest_min_schema_version"] == 2
    assert contract["zccache"]["local_dir_env"] == "SOLDR_ZCCACHE_LOCAL_DIR"
    assert contract["zccache"]["required_binaries"] == [
        "zccache",
        "zccache-daemon",
        "zccache-fp",
    ]
    assert contract["crgx"]["local_dir_env"] == "SOLDR_CRGX_LOCAL_DIR"
    assert contract["crgx"]["required_binaries"] == ["crgx"]


def test_python_contract_validates_release_manifest_sha256s(tmp_path: Path) -> None:
    module = _load_py_contract()
    manifest = _write_manifest_fixture(tmp_path)

    module.validate_release_manifest(
        manifest,
        soldr_target="x86_64-unknown-linux-gnu",
        windows=False,
        extract_dir=tmp_path,
    )

    manifest["zccache"]["binaries"][0]["sha256"] = "0" * 64
    with pytest.raises(RuntimeError, match="sha256 mismatch"):
        module.validate_release_manifest(
            manifest,
            soldr_target="x86_64-unknown-linux-gnu",
            windows=False,
            extract_dir=tmp_path,
        )


def test_python_action_helpers_import_contract_constants() -> None:
    module = _load_py_contract()
    ensure_spec = importlib.util.spec_from_file_location(
        "ensure_soldr",
        REPO_ROOT / ".github" / "actions" / "setup-soldr" / "ensure_soldr.py",
    )
    ensure_soldr = importlib.util.module_from_spec(ensure_spec)
    assert ensure_spec is not None
    assert ensure_spec.loader is not None
    ensure_spec.loader.exec_module(ensure_soldr)

    assert ensure_soldr.ARCHIVE_EXT == module.ARCHIVE_EXT
    assert ensure_soldr.ZCCACHE_BUNDLED_BINARIES == module.ZCCACHE_BUNDLED_BINARIES
    assert ensure_soldr.CRGX_BUNDLED_BINARY == module.CRGX_BUNDLED_BINARY


def test_release_workflow_and_docs_reference_contract_layout() -> None:
    contract = json.loads(CONTRACT_PATH.read_text(encoding="utf-8"))
    release_workflow = (REPO_ROOT / ".github" / "workflows" / "release-auto.yml").read_text(
        encoding="utf-8"
    )
    npm_docs = (REPO_ROOT / "docs" / "NPM_PUBLISHING.md").read_text(encoding="utf-8")
    runtime_docs = (REPO_ROOT / "docs" / "ZCCACHE_RUNTIME_CONTRACT.md").read_text(encoding="utf-8")

    assert '"schema_version": 2' in release_workflow
    assert '"format": "tar.zst"' in release_workflow
    for base in contract["release_archive"]["required_binaries"]:
        assert base in release_workflow
        assert base in npm_docs
    assert "contracts/zccache-runtime.v1.json" in npm_docs
    assert "contracts/zccache-runtime.v1.json" in runtime_docs


def test_npm_package_exports_contract_files() -> None:
    package = json.loads((REPO_ROOT / "package.json").read_text(encoding="utf-8"))

    assert "contracts/zccache-runtime.v1.json" in package["files"]
    assert "scripts/zccache-contract.js" in package["files"]
