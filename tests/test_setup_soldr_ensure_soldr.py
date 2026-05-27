from __future__ import annotations

import importlib.util
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = REPO_ROOT / ".github" / "actions" / "setup-soldr" / "ensure_soldr.py"


def _load_module():
    spec = importlib.util.spec_from_file_location("ensure_soldr", SCRIPT_PATH)
    module = importlib.util.module_from_spec(spec)
    assert spec is not None
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


def test_request_headers_include_github_token_when_present(monkeypatch) -> None:
    module = _load_module()
    monkeypatch.setenv("GITHUB_TOKEN", "test-token")

    headers = module._request_headers()

    assert headers["Authorization"] == "Bearer test-token"
    assert headers["User-Agent"] == "setup-soldr-action"


def test_request_headers_omit_authorization_when_token_missing(monkeypatch) -> None:
    module = _load_module()
    monkeypatch.delenv("GITHUB_TOKEN", raising=False)

    headers = module._request_headers()

    assert "Authorization" not in headers


def test_export_bundle_env_writes_local_dirs_when_bundle_present(
    tmp_path: Path,
    monkeypatch,
) -> None:
    module = _load_module()
    install_dir = tmp_path / "bin"
    install_dir.mkdir()
    github_env = tmp_path / "github.env"
    suffix = ".exe" if module.os.name == "nt" else ""

    for base in module.ZCCACHE_BUNDLED_BINARIES + (module.CRGX_BUNDLED_BINARY,):
        install_dir.joinpath(f"{base}{suffix}").write_text("binary", encoding="utf-8")

    monkeypatch.setenv("GITHUB_ENV", str(github_env))
    monkeypatch.delenv(module.ZCCACHE_LOCAL_DIR_ENV, raising=False)
    monkeypatch.delenv(module.CRGX_LOCAL_DIR_ENV, raising=False)

    module._export_bundle_env(install_dir)

    assert github_env.read_text(encoding="utf-8") == (
        f"{module.ZCCACHE_LOCAL_DIR_ENV}={install_dir}\n{module.CRGX_LOCAL_DIR_ENV}={install_dir}\n"
    )


def test_export_bundle_env_preserves_explicit_overrides(
    tmp_path: Path,
    monkeypatch,
) -> None:
    module = _load_module()
    install_dir = tmp_path / "bin"
    install_dir.mkdir()
    github_env = tmp_path / "github.env"
    suffix = ".exe" if module.os.name == "nt" else ""

    for base in module.ZCCACHE_BUNDLED_BINARIES + (module.CRGX_BUNDLED_BINARY,):
        install_dir.joinpath(f"{base}{suffix}").write_text("binary", encoding="utf-8")

    monkeypatch.setenv("GITHUB_ENV", str(github_env))
    monkeypatch.setenv(module.ZCCACHE_LOCAL_DIR_ENV, str(tmp_path / "zccache-override"))
    monkeypatch.setenv(module.CRGX_LOCAL_DIR_ENV, str(tmp_path / "crgx-override"))

    module._export_bundle_env(install_dir)

    assert github_env.read_text(encoding="utf-8") == ""
