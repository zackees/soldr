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
