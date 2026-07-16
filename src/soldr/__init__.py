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
import re
import subprocess
import sys
from pathlib import Path
from typing import Optional


_FAST_PROFILE_ENV = "SOLDR_PEP517_PROFILE"
_DISABLE_PROFILE_VALUES = {"", "none", "default", "off", "false", "0"}


def _project_root() -> Path:
    """Find the project root used by the PEP 517 invocation.

    PEP 517 frontends invoke the backend from the source tree, while the
    backend package itself lives in the isolated build environment. Walking
    from the current directory therefore works for both an in-tree backend
    test and an installed soldr wheel.
    """
    current = Path.cwd().resolve()
    for directory in (current, *current.parents):
        if (directory / "pyproject.toml").is_file():
            return directory
    return current


def _toml_section_values(path: Path, section: str) -> "dict[str, str]":
    """Read the small TOML subset needed before Python 3.11's tomllib.

    The backend must remain dependency-free in an isolated build environment.
    Use tomllib when available and a deliberately narrow fallback parser on
    Python 3.10. A malformed or unreadable optional config is ignored here;
    maturin/Cargo remains responsible for reporting the authoritative TOML
    error during the build.
    """
    try:
        import tomllib  # type: ignore[import-not-found]

        with path.open("rb") as stream:
            document = tomllib.load(stream)
        value = document
        for component in section.split("."):
            if not isinstance(value, dict):
                return {}
            value = value.get(component)
        if not isinstance(value, dict):
            return {}
        return {
            str(key): str(item)
            for key, item in value.items()
            if isinstance(item, (str, int, float, bool))
        }
    except (ImportError, OSError, ValueError):
        pass

    values: dict[str, str] = {}
    active = False
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except OSError:
        return values
    for line in lines:
        stripped = line.split("#", 1)[0].strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            active = stripped[1:-1].strip() == section
            continue
        if not active or "=" not in stripped:
            continue
        key, raw = (part.strip() for part in stripped.split("=", 1))
        if not re.fullmatch(r"[A-Za-z0-9_-]+", key):
            continue
        match = re.fullmatch(r"[\"'](.*)[\"']", raw)
        if match:
            values[key] = match.group(1)
        elif raw in {"true", "false"}:
            values[key] = raw
    return values


def _project_maturin_options() -> "dict[str, str]":
    return _toml_section_values(_project_root() / "pyproject.toml", "tool.maturin")


def _project_dev_profile_options() -> "dict[str, str]":
    return _toml_section_values(_project_root() / "Cargo.toml", "profile.dev")


def _setting_value(config_settings: Optional[dict], *keys: str) -> Optional[str]:
    if not config_settings:
        return None
    for key in keys:
        value = config_settings.get(key)
        if isinstance(value, (list, tuple)):
            value = value[-1] if value else None
        if value is not None and str(value).strip():
            return str(value).strip()
    return None


def _profile_args(
    config_settings: Optional[dict], *, editable: bool = False
) -> "list[str]":
    """Select the fast local profile without overriding explicit settings."""
    explicit = _setting_value(
        config_settings,
        "--profile",
        "profile",
        "editable-profile" if editable else "profile",
    )
    if explicit:
        return ["--profile", explicit]

    selected = os.environ.get(_FAST_PROFILE_ENV)
    if selected is not None:
        selected = selected.strip()
        if selected.lower() in _DISABLE_PROFILE_VALUES:
            return []
        return ["--profile", selected]

    options = _project_maturin_options()
    configured = options.get("editable-profile" if editable else "profile") or options.get(
        "profile"
    )
    if configured:
        return []

    return ["--profile", "dev"]


def _prep_env() -> "dict[str, str]":
    env = os.environ.copy()
    env.setdefault("RUSTC_WRAPPER", "soldr")
    env.setdefault("ZCCACHE_PATH_REMAP", "auto")
    # Ask soldr's maturin dispatch to use the automatic fast-linker policy.
    # An explicit SOLDR_LINKER value still wins in the Rust child.
    env.setdefault("SOLDR_PEP517_LINKER", "auto")
    # These are defaults for the backend-selected local `dev` profile. A
    # project-level Cargo profile setting wins by omission, and a caller-set
    # environment value always wins through setdefault. Release/custom
    # profiles are not affected.
    if not _project_dev_profile_options():
        env.setdefault("CARGO_PROFILE_DEV_DEBUG", "line-tables-only")
        env.setdefault("CARGO_PROFILE_DEV_LTO", "false")
        env.setdefault("CARGO_PROFILE_DEV_INCREMENTAL", "true")
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


def _target_args(config_settings: Optional[dict]) -> "list[str]":
    """Translate the PEP 517 target setting into maturin's explicit flag.

    Explicit config settings are the highest-precedence target source. When
    absent, the Rust-side shared plan resolves CARGO_BUILD_TARGET, then
    ``[tool.maturin].target``, then the host triple.
    """
    if not config_settings:
        return []
    for key in ("--target", "target", "build-target"):
        value = config_settings.get(key)
        if isinstance(value, (list, tuple)):
            value = value[-1] if value else None
        if value is not None and str(value).strip():
            return ["--target", str(value).strip()]
    return []


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
        raise RuntimeError(f"soldr build backend: no {suffix} {kind} produced in {directory}")
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
    target_args = _target_args(config_settings)
    _maturin_pep517(
        "write-dist-info",
        "--metadata-directory",
        metadata_directory,
        "--interpreter",
        sys.executable,
        *target_args,
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
    target_args = _target_args(config_settings)
    del metadata_directory
    _maturin_pep517(
        "build-wheel",
        "--interpreter",
        sys.executable,
        "--out",
        wheel_directory,
        *_profile_args(config_settings),
        *target_args,
    )
    return _newest_entry(wheel_directory, ".whl", want_dir=False)


def build_editable(
    wheel_directory: str,
    config_settings: Optional[dict] = None,
    metadata_directory: Optional[str] = None,
) -> str:
    target_args = _target_args(config_settings)
    del metadata_directory
    _maturin_pep517(
        "build-wheel",
        "--interpreter",
        sys.executable,
        "--out",
        wheel_directory,
        "--editable",
        *_profile_args(config_settings, editable=True),
        *target_args,
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
