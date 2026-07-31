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
    # Two suppressions, for two different blind spots, both structural:
    #
    # import-error: the module only becomes importable because of the
    # sys.path.insert on the line above, and no static checker models a
    # runtime path mutation. It resolves for anyone who happens to have the
    # `soldr` package installed and fails for everyone else -- which is a
    # check whose result depends on the machine, so it is silenced here
    # rather than left to fail on whichever host lacks the install.
    #
    # import-untyped: the package ships no py.typed marker, so mypy will not
    # read its annotations. Declaring `soldr` typed is a packaging decision
    # with downstream reach, and a dev shim is not the place to make it.
    # pylint: disable-next=import-error
    from soldr.setup_soldr_exporter import main  # type: ignore[import-untyped]

    raise SystemExit(main())
