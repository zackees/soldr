"""soldr — Instant tools. Instant builds.

PEP 517 build backend that delegates to a managed maturin binary.

Set in your pyproject.toml::

    [build-system]
    requires = ["soldr"]
    build-backend = "soldr"

soldr fetches a pinned maturin from GitHub Releases on first build
(cached under ~/.soldr/bin/) and delegates to it via
``soldr maturin pep517 <hook>``. Builds run with ``RUSTC_WRAPPER=soldr``
and ``ZCCACHE_PATH_REMAP=auto`` so rustc invocations are cached and
git-worktree caches share via path normalization.

``requires = ["soldr"]`` alone is sufficient — there is no separate
``maturin`` Python dependency to add, because soldr handles maturin
acquisition itself.
"""

import os
import subprocess
import sys
from pathlib import Path
from typing import Optional


def _prep_env() -> "dict[str, str]":
    env = os.environ.copy()
    env.setdefault("RUSTC_WRAPPER", "soldr")
    env.setdefault("ZCCACHE_PATH_REMAP", "auto")
    # Stable CARGO_TARGET_DIR for PEP 517 isolated builds. When pip/uv
    # build from an sdist they copy the sources to a throwaway temp dir,
    # so `<srcdir>/target/` is discarded after every build and cargo
    # runs cold each time (25-30s+ per `pip install`). Pinning the
    # target dir to a stable per-user path keeps cargo's incremental
    # fingerprint cache hot across isolated builds.
    #
    # Ingested from FastLED/fbuild's setup.py (`WHEEL_BUILD_TARGET_DIR`,
    # FastLED/fbuild#829): one shared `wheel-build` dir, deliberately
    # separate from any dev `<repo>/target/` so `pip install` and the
    # dev CLI don't invalidate each other's artifacts. Cargo keys
    # artifacts by package, so sharing one dir across projects is safe.
    #
    # Escape hatches: a caller-provided CARGO_TARGET_DIR always wins
    # (setdefault), and SOLDR_PEP517_STABLE_TARGET_DIR=0 (or false/no/
    # off) skips the pin entirely.
    knob = env.get("SOLDR_PEP517_STABLE_TARGET_DIR", "").strip().lower()
    if knob not in ("0", "false", "no", "off"):
        env.setdefault(
            "CARGO_TARGET_DIR",
            str(Path.home() / ".soldr" / "cargo-target" / "wheel-build"),
        )
    return env


def _maturin_pep517(subcommand: str, *args: str) -> None:
    cmd = ["soldr", "maturin", "pep517", subcommand, *args]
    try:
        subprocess.check_call(cmd, env=_prep_env(), timeout=600)
    except subprocess.TimeoutExpired as exc:
        raise RuntimeError(
            "soldr maturin pep517 exceeded 600s; suspect zccache daemon wedge - "
            "try `soldr status` to inspect."
        ) from exc


def _newest_entry(directory: str, suffix: str, *, want_dir: bool) -> str:
    entries = []
    for name in os.listdir(directory):
        if not name.endswith(suffix):
            continue
        path = Path(directory, name)
        if want_dir and not path.is_dir():
            continue
        if not want_dir and not path.is_file():
            continue
        entries.append((path.stat().st_mtime, name))
    if not entries:
        kind = "directory" if want_dir else "file"
        raise RuntimeError(
            f"soldr build backend: no {suffix} {kind} produced in {directory}"
        )
    entries.sort(reverse=True)
    return entries[0][1]


# PEP 517 dictates the exact parameter names below; renaming them with
# `_` prefixes to silence linters would break frontends that call by
# keyword. `del` after entry preserves the contract while marking each
# unused arg as intentionally ignored for pylint W0613 / pyright
# reportUnusedParameter / ruff ARG001.


def get_requires_for_build_wheel(config_settings: Optional[dict] = None):
    del config_settings
    return []


def get_requires_for_build_sdist(config_settings: Optional[dict] = None):
    del config_settings
    return []


def get_requires_for_build_editable(config_settings: Optional[dict] = None):
    del config_settings
    return []


def prepare_metadata_for_build_wheel(
    metadata_directory: str,
    config_settings: Optional[dict] = None,
) -> str:
    del config_settings
    _maturin_pep517(
        "write-dist-info",
        "--metadata-directory",
        metadata_directory,
        "--interpreter",
        sys.executable,
    )
    return _newest_entry(metadata_directory, ".dist-info", want_dir=True)


prepare_metadata_for_build_editable = prepare_metadata_for_build_wheel


def build_wheel(
    wheel_directory: str,
    config_settings: Optional[dict] = None,
    metadata_directory: Optional[str] = None,
) -> str:
    # maturin pep517 build-wheel does not accept --metadata-directory;
    # the dist-info is regenerated, which PEP 517 explicitly permits.
    del config_settings, metadata_directory
    _maturin_pep517(
        "build-wheel",
        "--interpreter",
        sys.executable,
        "--out",
        wheel_directory,
    )
    return _newest_entry(wheel_directory, ".whl", want_dir=False)


def build_editable(
    wheel_directory: str,
    config_settings: Optional[dict] = None,
    metadata_directory: Optional[str] = None,
) -> str:
    del config_settings, metadata_directory
    _maturin_pep517(
        "build-wheel",
        "--interpreter",
        sys.executable,
        "--out",
        wheel_directory,
        "--editable",
    )
    return _newest_entry(wheel_directory, ".whl", want_dir=False)


def build_sdist(
    sdist_directory: str,
    config_settings: Optional[dict] = None,
) -> str:
    del config_settings
    _maturin_pep517(
        "write-sdist",
        "--sdist-directory",
        sdist_directory,
    )
    return _newest_entry(sdist_directory, ".tar.gz", want_dir=False)
