import importlib.metadata
import re
from pathlib import Path


def _extract_version(pattern: str, text: str) -> str:
    match = re.search(pattern, text, re.MULTILINE)
    assert match is not None
    return match.group(1)


def test_python_and_cargo_versions_match() -> None:
    root = Path(__file__).resolve().parents[1]
    init_text = (root / "src" / "running_process" / "__init__.py").read_text(encoding="utf-8")
    pyproject_text = (root / "pyproject.toml").read_text(encoding="utf-8")
    cargo_text = (root / "Cargo.toml").read_text(encoding="utf-8")

    init_version = _extract_version(r'^__version__\s*=\s*"([^"]+)"', init_text)
    pyproject_version = _extract_version(r'^version\s*=\s*"([^"]+)"', pyproject_text)
    cargo_version = _extract_version(r'^version\s*=\s*"([^"]+)"', cargo_text)

    assert init_version == pyproject_version == cargo_version


def test_installed_distribution_version_matches_module_version() -> None:
    import running_process

    assert running_process.__version__ == importlib.metadata.version("running-process")
