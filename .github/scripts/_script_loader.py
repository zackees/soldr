"""Shared helper for the guards that live beside the scripts they cover.

The scripts in this directory are executed by CI as files, not imported as a
package, so there is no `import` statement that reaches them. Every guard here
hand-rolled the same importlib dance; this is that dance, once (soldr#2120).

This deliberately duplicates `tests/conftest.py::load_script_module` rather
than importing it. `tests/` and `.github/scripts/` are separate pytest roots
with no import path between them, so sharing would mean bootstrapping one
conftest from the other *with the very dance being shared*. Ten duplicated
lines across two roots is the cheaper trade, and both copies carry this note.

It is a plain module rather than a second `conftest.py` on purpose. pytest
imports every conftest as the top-level name `conftest`, so a run that
collects both roots would have the two files fight over that name and
`tests/` would import this one's contents. That is not hypothetical -- it is
what happened, and the suite failed collection until this was renamed.
"""

from __future__ import annotations

import importlib.util
import sys
from pathlib import Path
from types import ModuleType


def load_script_module(path: str | Path, name: str | None = None) -> ModuleType:
    """Import a standalone script as a module.

    `name` defaults to the file stem. The module is registered in `sys.modules`
    *before* `exec_module` runs, which some of the guarded scripts require: a
    dataclass resolves its own `__module__` through `sys.modules` while the
    class is being created, and raises `KeyError` if the entry is missing.
    """

    script = Path(path)
    module_name = name or script.stem
    spec = importlib.util.spec_from_file_location(module_name, script)
    if spec is None or spec.loader is None:
        raise ImportError(f"cannot load {script} as a module")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def load_sibling_script(name: str) -> ModuleType:
    """Import `<name>.py` from this directory.

    The common case here: a guard named `test_foo.py` loading `foo.py` next to
    it. Resolving relative to this file rather than the caller keeps it correct
    regardless of the working directory CI happens to use.
    """

    return load_script_module(Path(__file__).with_name(f"{name}.py"), name)
