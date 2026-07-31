#!/usr/bin/env python3
"""Dev shim: run the setup-soldr action exporter from a checkout.

The exporter lives in the shipped `soldr` package under `src/`, which is not on
`sys.path` for someone running this straight out of a clone. Both the path
insert and the import therefore happen inside `__main__`: at module scope the
import necessarily follows a statement, and no suppression makes that untrue.
"""

from __future__ import annotations

import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]


if __name__ == "__main__":
    sys.path.insert(0, str(REPO_ROOT / "src"))
    # The `soldr` package ships no py.typed marker, so mypy will not read its
    # annotations. Declaring the package typed is a packaging decision with
    # downstream reach; a dev shim is not the place to make it.
    from soldr.setup_soldr_exporter import main  # type: ignore[import-untyped]

    raise SystemExit(main())
