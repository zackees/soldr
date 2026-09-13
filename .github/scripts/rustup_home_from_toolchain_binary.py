#!/usr/bin/env python3
"""Print the RUSTUP_HOME a provisioned toolchain binary lives in (soldr#3195).

The target-run lane provisions the pinned toolchain, then exports `CARGO`,
`RUSTC` and `RUSTUP_TOOLCHAIN`. It did not export `RUSTUP_HOME`. Fixtures that
isolate a test by pointing `HOME` at a temp directory then made rustup look for
the toolchain under that empty home, and rustup quietly installed a full copy
inside each such test: on the aarch64 lane, three daemon tests each downloaded
1.95.0 while passing. With the Nextest wrapper refusing test-time toolchain
downloads, those tests failed instead with
`toolchain '1.95.0-aarch64-unknown-linux-gnu' is not installed`.

`rustup which cargo` already names the provisioned binary, whose shape is
`<RUSTUP_HOME>/toolchains/<toolchain>/bin/cargo`. This reads the home back out
of that path. Anything else is refused rather than guessed: exporting a wrong
`RUSTUP_HOME` would redirect every child to a toolchain store nobody verified.

Usage:
    python .github/scripts/rustup_home_from_toolchain_binary.py <cargo-or-rustc path>
"""

from __future__ import annotations

import argparse
import sys
from pathlib import PurePath, PurePosixPath, PureWindowsPath


def rustup_home_from(binary: str) -> str | None:
    """The RUSTUP_HOME containing `binary`, or None if its shape is not
    `<home>/toolchains/<toolchain>/bin/<tool>`."""

    path: PurePath = PureWindowsPath(binary) if "\\" in binary else PurePosixPath(binary)
    parts = path.parts
    if len(parts) < 5 or parts[-2] != "bin" or parts[-4] != "toolchains":
        return None
    if not parts[-3] or not parts[-1]:
        return None
    home = path.parents[3]
    return str(home) if str(home) not in ("", ".") else None


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("binary", help="absolute path to a provisioned cargo or rustc")
    args = parser.parse_args(argv)
    home = rustup_home_from(args.binary.strip())
    if home is None:
        print(
            f"rustup_home_from_toolchain_binary: {args.binary!r} is not "
            "<RUSTUP_HOME>/toolchains/<toolchain>/bin/<tool>; refusing to guess",
            file=sys.stderr,
        )
        return 1
    print(home)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
