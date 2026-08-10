"""Cargo / maturin command helpers.

`cargo_command` routes through `soldr cargo …` when the `soldr` binary
is on PATH, and falls back to raw `cargo` otherwise. This keeps the
project's stated toolchain policy (CLAUDE.md "soldr-prefixed build
commands") honest from Python, matches the conditional already used in
`install:248`, and degrades cleanly on CI runners that use
`dtolnay/rust-toolchain` + `Swatinem/rust-cache` without soldr installed.

Historical note: this module used to wrap `cargo` with `soldr cargo …`
unconditionally. After zccache caused macOS-only build-script failures
(PR #116), CI was switched to the standard rust-toolchain / rust-cache
combo and `cargo_command` was reduced to a passthrough — which made
local developer toolchain hygiene rely on whatever `cargo` was first on
PATH. The conditional restored here gives soldr's hygiene back without
breaking CI.
"""

from __future__ import annotations

import shutil


def cargo_command(*args: str) -> list[str]:
    if shutil.which("soldr"):
        return ["soldr", "cargo", *args]
    return ["cargo", *args]


def maturin_command(python: str, *args: str) -> list[str]:
    return [python, "-m", "maturin", *args]
