from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import pytest
from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
validate = load_script_module(
    REPO_ROOT / ".github" / "scripts" / "validate_npm_release_recovery.py"
)


def _source(tmp_path: Path, version: str = "0.9.0") -> Path:
    (tmp_path / "package.json").write_text(
        json.dumps({"version": version}), encoding="utf-8"
    )
    (tmp_path / "Cargo.toml").write_text(
        f'[workspace.package]\nversion = "{version}"\n', encoding="utf-8"
    )
    (tmp_path / "Cargo.lock").write_text(
        f'[[package]]\nname = "soldr-cli"\nversion = "{version}"\n',
        encoding="utf-8",
    )
    return tmp_path


def _release(version: str = "0.9.0") -> dict[str, Any]:
    names = [
        f"soldr-v{version}-SHA256SUMS.txt",
        f"soldr-v{version}-x86_64-pc-windows-msvc.tar.zst",
        f"soldr-{version}-py3-none-win_amd64.whl",
    ]
    return {
        "tag_name": f"v{version}",
        "draft": False,
        "immutable": True,
        "target_commitish": "a" * 40,
        "assets": [{"name": name} for name in names],
    }


def _pypi(version: str = "0.9.0") -> dict[str, Any]:
    return {
        "info": {"version": version},
        "urls": [
            {
                "filename": f"soldr-{version}-py3-none-win_amd64.whl",
                "packagetype": "bdist_wheel",
            }
        ],
    }


def _git(_source_dir: Path, arguments: list[str]) -> str:
    assert arguments[:2] == ["rev-parse", "--verify"]
    return "a" * 40


def _fetcher(release: dict[str, Any], pypi: dict[str, Any]):
    def fetch(url: str, _token: str | None) -> dict[str, Any]:
        return release if "api.github.com" in url else pypi

    return fetch


def test_accepts_matching_immutable_release(tmp_path: Path) -> None:
    version = validate.validate_recovery(
        repository="zackees/soldr",
        release_ref="v0.9.0",
        source_dir=_source(tmp_path),
        token="token",
        get_json=_fetcher(_release(), _pypi()),
        run_git=_git,
    )

    assert version == "0.9.0"


@pytest.mark.parametrize("release_ref", ["main", "release/0.9.0", "v0.9.0-rc.1"])
def test_rejects_branch_or_non_stable_refs(tmp_path: Path, release_ref: str) -> None:
    with pytest.raises(validate.ValidationError, match=r"exact stable vX\.Y\.Z tag"):
        validate.validate_recovery(
            repository="zackees/soldr",
            release_ref=release_ref,
            source_dir=_source(tmp_path),
            token=None,
            get_json=_fetcher(_release(), _pypi()),
            run_git=_git,
        )


def test_rejects_missing_local_tag(tmp_path: Path) -> None:
    def missing_tag(source_dir: Path, arguments: list[str]) -> str:
        if arguments[-1].startswith("refs/tags/"):
            raise validate.ValidationError("tag is missing")
        return _git(source_dir, arguments)

    with pytest.raises(validate.ValidationError, match="tag is missing"):
        validate.validate_recovery(
            repository="zackees/soldr",
            release_ref="v0.9.0",
            source_dir=_source(tmp_path),
            token=None,
            get_json=_fetcher(_release(), _pypi()),
            run_git=missing_tag,
        )


def test_rejects_tag_version_mismatch(tmp_path: Path) -> None:
    with pytest.raises(validate.ValidationError, match="versions must match"):
        validate.validate_recovery(
            repository="zackees/soldr",
            release_ref="v0.9.1",
            source_dir=_source(tmp_path),
            token=None,
            get_json=_fetcher(_release(), _pypi()),
            run_git=_git,
        )


@pytest.mark.parametrize(
    ("field", "value", "message"),
    [("draft", True, "published"), ("immutable", False, "immutable")],
)
def test_rejects_unpublished_or_mutable_release(
    tmp_path: Path, field: str, value: bool, message: str
) -> None:
    release = _release()
    release[field] = value
    with pytest.raises(validate.ValidationError, match=message):
        validate.validate_recovery(
            repository="zackees/soldr",
            release_ref="v0.9.0",
            source_dir=_source(tmp_path),
            token=None,
            get_json=_fetcher(release, _pypi()),
            run_git=_git,
        )


def test_rejects_github_pypi_wheel_mismatch(tmp_path: Path) -> None:
    pypi = _pypi()
    pypi["urls"][0]["filename"] = "soldr-0.9.0-py3-none-manylinux.whl"
    with pytest.raises(validate.ValidationError, match="wheel sets do not match"):
        validate.validate_recovery(
            repository="zackees/soldr",
            release_ref="v0.9.0",
            source_dir=_source(tmp_path),
            token=None,
            get_json=_fetcher(_release(), pypi),
            run_git=_git,
        )
