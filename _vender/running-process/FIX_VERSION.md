# Fix Version Sync

## Problem

A previously published `running-process` build had a version mismatch:

- package metadata / installed distribution version was one value
- `running_process.__version__` in `src/running_process/__init__.py` was still an older value

That makes downstream debugging confusing because:

- `importlib.metadata.version("running-process")` reports one version
- `running_process.__version__` reports another

## Test to add

Add a unit test that fails whenever the recorded module version drifts from the package manifest version.

Suggested test shape:

```python
from __future__ import annotations

import re
from pathlib import Path


def _extract_pyproject_version(pyproject_text: str) -> str:
    match = re.search(r'^version\\s*=\\s*"([^"]+)"', pyproject_text, re.MULTILINE)
    assert match is not None
    return match.group(1)


def _extract_init_version(init_text: str) -> str:
    match = re.search(r'^__version__\\s*=\\s*"([^"]+)"', init_text, re.MULTILINE)
    assert match is not None
    return match.group(1)


def test_module_version_matches_pyproject() -> None:
    root = Path(__file__).resolve().parents[1]
    pyproject_text = (root / "pyproject.toml").read_text(encoding="utf-8")
    init_text = (root / "src" / "running_process" / "__init__.py").read_text(encoding="utf-8")

    assert _extract_init_version(init_text) == _extract_pyproject_version(pyproject_text)
```

## Why this test

This catches the exact class of regression where:

- packaging metadata is bumped
- source-level exported version string is not bumped

It should run as a fast unit test and does not require building or installing the package.

## Optional stronger version

If the test suite already builds an installed artifact during CI, add a second check:

- compare `running_process.__version__`
- compare `importlib.metadata.version("running-process")`
- assert they match too

That would catch both:

- source mismatch
- packaging/install mismatch
