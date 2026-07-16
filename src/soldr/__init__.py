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

import hashlib
import importlib
import json
import os
import re
import shutil
import subprocess
import sys
import sysconfig
import time
from contextlib import contextmanager
from pathlib import Path
from typing import Iterator, Optional

_FAST_PROFILE_ENV = "SOLDR_PEP517_PROFILE"
_STATS_ENV = "SOLDR_PEP517_STATS"
_WHEEL_CACHE_ENV = "SOLDR_PEP517_WHEEL_CACHE"
_DISABLE_PROFILE_VALUES = {"", "none", "default", "off", "false", "0"}
_DELEGATE_BACKEND_SECTION = "tool.soldr.pep517"
_PEP517_ENV_KEYS = {
    "RUSTC_WRAPPER",
    "ZCCACHE_PATH_REMAP",
    "SOLDR_PEP517_LINKER",
    "SOLDR_PEP517_PROJECT_ID",
    "CARGO_TARGET_DIR",
    "CARGO_PROFILE_DEV_OPT_LEVEL",
    "CARGO_PROFILE_DEV_CODEGEN_UNITS",
    "CARGO_PROFILE_DEV_DEBUG",
    "CARGO_PROFILE_DEV_LTO",
    "CARGO_PROFILE_DEV_INCREMENTAL",
    "CARGO_BUILD_TARGET",
    "CARGO_ENCODED_RUSTFLAGS",
    "RUSTFLAGS",
    "SOLDR_PEP517_PROFILE",
    "SOLDR_LINKER",
}
_MISSING = object()
_PEP517_TARGET_SCHEMA = b"pep517-target-v3"
_WHEEL_CACHE_SCHEMA = b"pep517-wheel-cache-v1"
_WHEEL_CACHE_IGNORED_DIRECTORIES = {
    ".git",
    ".hg",
    ".mypy_cache",
    ".nox",
    ".pytest_cache",
    ".ruff_cache",
    ".svn",
    ".tox",
    ".venv",
    "__pycache__",
    "node_modules",
    "target",
    "venv",
}
_FAST_DEV_PROFILE_DEFAULTS = {
    "CARGO_PROFILE_DEV_OPT_LEVEL": ("opt-level", "0"),
    "CARGO_PROFILE_DEV_CODEGEN_UNITS": ("codegen-units", "256"),
    "CARGO_PROFILE_DEV_DEBUG": ("debug", "line-tables-only"),
    "CARGO_PROFILE_DEV_LTO": ("lto", "false"),
    "CARGO_PROFILE_DEV_INCREMENTAL": ("incremental", "true"),
}


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


def _project_soldr_options() -> "dict[str, str]":
    return _toml_section_values(
        _project_root() / "pyproject.toml", _DELEGATE_BACKEND_SECTION
    )


def _delegate_backend_name() -> Optional[str]:
    value = _project_soldr_options().get("delegate-backend")
    return value.strip() if value and value.strip() else None


def _delegate_backend() -> object | None:
    name = _delegate_backend_name()
    if not name:
        return None
    module_name, separator, attribute = name.partition(":")
    if module_name == "soldr" or module_name.startswith("soldr."):
        raise RuntimeError(
            "soldr PEP 517 delegate-backend cannot delegate back to soldr; "
            "select a concrete backend such as setuptools.build_meta"
        )
    backend = importlib.import_module(module_name)
    if separator and attribute:
        return getattr(backend, attribute)
    if separator:
        raise RuntimeError(f"invalid soldr delegate-backend `{name}`")
    return backend


@contextmanager
def _managed_pep517_environment(
    config_settings: Optional[dict] = None,
    *,
    editable: bool = False,
) -> Iterator[None]:
    """Apply soldr's child-build environment around a delegated backend."""
    prepared = _prep_env(config_settings, editable=editable)
    previous: dict[str, object] = {
        key: os.environ.get(key, _MISSING) for key in _PEP517_ENV_KEYS
    }
    try:
        for key in _PEP517_ENV_KEYS:
            value = prepared.get(key)
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value
        yield
    finally:
        for key, value in previous.items():
            if value is _MISSING:
                os.environ.pop(key, None)
            else:
                os.environ[key] = str(value)


def _delegate_hook(
    name: str,
    *args: object,
    fallback: Optional[str] = None,
    _config_settings: Optional[dict] = None,
    **kwargs: object,
):
    backend = _delegate_backend()
    if backend is None:
        return None
    hook_name = name if hasattr(backend, name) else fallback
    if hook_name is None or not hasattr(backend, hook_name):
        return [] if name.startswith("get_requires_for_build_") else None
    with _managed_pep517_environment(
        _config_settings,
        editable=name.endswith("_editable"),
    ):
        return getattr(backend, hook_name)(*args, **kwargs)


def _hash_identity_field(hasher: "hashlib._Hash", name: str, value: bytes) -> None:
    name_bytes = name.encode("utf-8")
    hasher.update(len(name_bytes).to_bytes(8, "little"))
    hasher.update(name_bytes)
    hasher.update(len(value).to_bytes(8, "little"))
    hasher.update(value)


def _project_build_identity(environment: Optional[dict[str, str]] = None) -> str:
    """Return a path-independent identity for the PEP build configuration.

    This selects a stable target namespace; it is not a replacement for
    Cargo's fingerprints. Rust source changes intentionally keep the same
    namespace so Cargo can reuse valid artifacts and decide what is stale.
    """
    root = _project_root()
    hasher = hashlib.sha256()
    _hash_identity_field(hasher, "schema", _PEP517_TARGET_SCHEMA)
    configuration_files = [
        "pyproject.toml",
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        "rust-toolchain",
        ".cargo/config.toml",
        ".cargo/config",
    ]
    configuration_files.extend(
        str(path.relative_to(root)).replace("\\", "/")
        for path in sorted(root.glob("crates/**/Cargo.toml"))
    )
    for relative in configuration_files:
        path = root / relative
        try:
            contents = path.read_bytes()
        except OSError:
            continue
        _hash_identity_field(hasher, relative, contents)
    values = os.environ if environment is None else environment
    for name in (
        "CARGO_BUILD_TARGET",
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTFLAGS",
        "SOLDR_PEP517_PROFILE",
        "SOLDR_PEP517_LINKER",
        "SOLDR_LINKER",
        "CARGO_PROFILE_DEV_OPT_LEVEL",
        "CARGO_PROFILE_DEV_CODEGEN_UNITS",
        "CARGO_PROFILE_DEV_DEBUG",
        "CARGO_PROFILE_DEV_LTO",
        "CARGO_PROFILE_DEV_INCREMENTAL",
    ):
        _hash_identity_field(hasher, name, values.get(name, "").encode())
    return hasher.hexdigest()[:24]


def _pep517_target_dir(project_id: Optional[str] = None) -> Path:
    if project_id is None:
        project_id = _project_build_identity()
    return Path.home() / ".soldr" / "cargo-target" / "pep517" / project_id


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


def _explicit_profile(
    config_settings: Optional[dict], *, editable: bool = False
) -> Optional[str]:
    keys = ("--profile", "profile")
    if editable:
        keys += ("editable-profile",)
    return _setting_value(config_settings, *keys)


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
    configured = options.get(
        "editable-profile" if editable else "profile"
    ) or options.get("profile")
    if configured:
        return []

    return ["--profile", "dev"]


def _prep_env(
    config_settings: Optional[dict] = None,
    *,
    editable: bool = False,
) -> "dict[str, str]":
    env = os.environ.copy()
    explicit_profile = _explicit_profile(config_settings, editable=editable)
    if explicit_profile:
        # Match _profile_args precedence for delegated backends: an explicit
        # PEP setting wins over SOLDR_PEP517_PROFILE from the caller. This
        # value is also included in the project identity below so dev/release
        # delegated builds never share an ambiguous target namespace.
        env[_FAST_PROFILE_ENV] = explicit_profile
    identity_environment = os.environ.copy()
    if explicit_profile:
        identity_environment[_FAST_PROFILE_ENV] = explicit_profile
    env.setdefault("RUSTC_WRAPPER", "soldr")
    env.setdefault("ZCCACHE_PATH_REMAP", "auto")
    # Ask soldr's maturin dispatch to use the automatic fast-linker policy.
    # An explicit SOLDR_LINKER value still wins in the Rust child.
    env.setdefault("SOLDR_PEP517_LINKER", "auto")
    # These are defaults for the backend-selected local `dev` profile. A
    # project-level Cargo setting wins per field, and a caller-set environment
    # value always wins through setdefault. Release/custom profiles are not
    # affected.
    project_dev_options = _project_dev_profile_options()
    for environment_key, (cargo_key, default) in _FAST_DEV_PROFILE_DEFAULTS.items():
        if cargo_key not in project_dev_options:
            env.setdefault(environment_key, default)
    # Stable CARGO_TARGET_DIR for PEP 517 isolated builds. When pip/uv
    # build from an sdist they copy the sources to a throwaway temp dir,
    # so `<srcdir>/target/` is discarded after every build and cargo
    # runs cold each time (25-30s+ per `pip install`). Pinning the
    # target dir to a stable per-user path keeps cargo's incremental
    # fingerprint cache hot across isolated builds.
    #
    # Ingested from FastLED/fbuild's setup.py (`WHEEL_BUILD_TARGET_DIR`,
    # FastLED/fbuild#829): keep PEP builds separate from any dev
    # `<repo>/target/` so `pip install` and the dev CLI do not invalidate
    # each other's artifacts. The namespace is content-derived from the
    # project build configuration, so temporary PEP source directories
    # reuse the same target while unrelated projects do not share state.
    #
    # Escape hatches: a caller-provided CARGO_TARGET_DIR always wins
    # (setdefault), and SOLDR_PEP517_STABLE_TARGET_DIR=0 (or false/no/
    # off) skips the pin entirely.
    project_id = _project_build_identity(identity_environment)
    env.setdefault("SOLDR_PEP517_PROJECT_ID", project_id)
    knob = env.get("SOLDR_PEP517_STABLE_TARGET_DIR", "").strip().lower()
    if knob not in ("0", "false", "no", "off"):
        env.setdefault(
            "CARGO_TARGET_DIR",
            str(_pep517_target_dir(project_id)),
        )
    return env


def _frontend_is_verbose(env: "dict[str, str]") -> bool:
    """Best-effort verbosity detection for common PEP 517 frontends."""
    for key in ("PIP_VERBOSE", "UV_VERBOSE"):
        value = env.get(key, "").strip().lower()
        if value and value not in _DISABLE_PROFILE_VALUES:
            return True
    return False


def _stats_mode(env: "dict[str, str]") -> str:
    raw = env.get(_STATS_ENV)
    if raw is None:
        return "full" if _frontend_is_verbose(env) else "short"
    value = raw.strip().lower()
    if value in _DISABLE_PROFILE_VALUES:
        return "off"
    if value == "full":
        return "full"
    return "short"


def _session_command(
    subcommand: str, env: "dict[str, str]", *args: str
) -> "dict | None":
    """Run a best-effort session command without perturbing a wheel build."""
    try:
        result = subprocess.run(
            ["soldr", subcommand, *args, "--json"],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )
    except OSError:
        return None
    if result.returncode != 0:
        return None
    try:
        payload = json.loads(result.stdout)
    except (TypeError, ValueError):
        return None
    return payload if isinstance(payload, dict) else None


def _emit_build_stats(
    env: "dict[str, str]",
    session_id: "str | None",
    elapsed_seconds: float,
    label: str,
) -> None:
    mode = _stats_mode(env)
    if mode == "off":
        return

    end = (
        _session_command("session-end", env, "--id", session_id)
        if session_id is not None
        else None
    )
    stats = end.get("stats") if end else None
    elapsed = f"{elapsed_seconds:.1f}s"
    if not isinstance(stats, dict):
        print(
            f"soldr PEP 517: built {label} in {elapsed} | cache stats unavailable",
            file=sys.stderr,
        )
        return

    hits = stats.get("hits", 0)
    misses = stats.get("misses", 0)
    hit_rate = stats.get("hit_rate", 0.0)
    saved_ms = stats.get("time_saved_ms", 0)
    try:
        rate = float(hit_rate) * 100.0
    except (TypeError, ValueError):
        rate = 0.0
    try:
        saved = float(saved_ms) / 1000.0
    except (TypeError, ValueError):
        saved = 0.0
    print(
        f"soldr PEP 517: built {label} in {elapsed} | "
        f"cache {hits} hits / {misses} misses ({rate:.1f}%) | "
        f"saved {saved:.1f}s",
        file=sys.stderr,
    )
    if mode == "full":
        print(
            "soldr PEP 517 details: "
            + json.dumps(
                {"build_seconds": round(elapsed_seconds, 3), "cache": stats},
                sort_keys=True,
            ),
            file=sys.stderr,
        )


def _maturin_pep517(
    subcommand: str, *args: str, build_label: "str | None" = None
) -> None:
    env = _prep_env()
    mode = _stats_mode(env)
    started_at = time.perf_counter()
    start = (
        _session_command("session-start", env)
        if build_label and mode != "off"
        else None
    )
    session_id = start.get("session_id") if start else None
    if isinstance(session_id, str):
        env["ZCCACHE_SESSION_ID"] = session_id
    else:
        session_id = None
    cmd = ["soldr", "maturin", "pep517", subcommand, *args]
    try:
        subprocess.check_call(cmd, env=env, timeout=600)
    except subprocess.TimeoutExpired as exc:
        if session_id is not None:
            _session_command("session-end", env, "--id", session_id)
        raise RuntimeError(
            "soldr maturin pep517 exceeded 600s; suspect zccache daemon wedge - "
            "try `soldr status` to inspect."
        ) from exc
    except Exception:
        if session_id is not None:
            _session_command("session-end", env, "--id", session_id)
        raise
    if build_label:
        _emit_build_stats(
            env,
            session_id,
            time.perf_counter() - started_at,
            build_label,
        )


def _wheel_cache_root(environment: "dict[str, str]") -> Path:
    root = environment.get("SOLDR_CACHE_DIR")
    return Path(root).expanduser() if root else Path.home() / ".soldr"


def _hash_metadata_tree(
    hasher: "hashlib._Hash",
    root: Path,
    label: str,
    ignored_directories: "set[str] | None" = None,
) -> None:
    """Hash paths plus metadata without reading staged native artifacts."""
    try:
        root = root.resolve()
    except OSError:
        return
    if not root.is_dir():
        _hash_identity_field(hasher, label, b"missing")
        return
    ignored = _WHEEL_CACHE_IGNORED_DIRECTORIES | (ignored_directories or set())

    for directory, directories, files in os.walk(root):
        directories[:] = sorted(item for item in directories if item not in ignored)
        current = Path(directory)
        for filename in sorted(files):
            path = current / filename
            try:
                relative = path.relative_to(root).as_posix()
                stat = path.stat()
            except OSError:
                continue
            if not path.is_file():
                continue
            _hash_identity_field(
                hasher,
                f"{label}/{relative}",
                f"{stat.st_size}:{stat.st_mtime_ns}".encode(),
            )


def _hash_metadata_directory(
    hasher: "hashlib._Hash", metadata_directory: Optional[str]
) -> None:
    """Hash PEP 517 prepared metadata by content, not its temporary path."""
    if not metadata_directory:
        _hash_identity_field(hasher, "metadata", b"none")
        return
    root = Path(metadata_directory)
    if not root.is_dir():
        _hash_identity_field(hasher, "metadata", b"missing")
        return
    for directory, _, files in os.walk(root):
        current = Path(directory)
        for filename in sorted(files):
            path = current / filename
            try:
                relative = path.relative_to(root).as_posix()
                contents = path.read_bytes()
            except OSError:
                continue
            _hash_identity_field(hasher, f"metadata/{relative}", contents)


def _delegate_backend_stamp() -> str:
    """Return a version marker without baking an isolated-env path into a key."""
    name = _delegate_backend_name()
    if not name:
        return "maturin"
    module_name = name.partition(":")[0]
    package = module_name.split(".", 1)[0]
    try:
        from importlib import metadata
    except ImportError:
        return name

    try:
        versions = metadata.packages_distributions().get(package, [])
        return (
            ";".join(
                f"{distribution}={metadata.version(distribution)}"
                for distribution in sorted(versions)
            )
            or name
        )
    except metadata.PackageNotFoundError:
        return name


def _wheel_cache_context(
    kind: str,
    config_settings: Optional[dict],
    metadata_directory: Optional[str],
) -> "tuple[Path, str] | None":
    environment = _prep_env(config_settings, editable=kind == "editable")
    cache_knob = environment.get(_WHEEL_CACHE_ENV)
    if cache_knob is not None and cache_knob.strip().lower() in _DISABLE_PROFILE_VALUES:
        return None

    hasher = hashlib.sha256()
    _hash_identity_field(hasher, "schema", _WHEEL_CACHE_SCHEMA)
    _hash_identity_field(hasher, "kind", kind.encode())
    _hash_identity_field(
        hasher,
        "config-settings",
        json.dumps(config_settings or {}, sort_keys=True, default=str).encode(),
    )
    _hash_identity_field(hasher, "backend", _delegate_backend_stamp().encode())
    _hash_identity_field(
        hasher,
        "python",
        f"{sys.implementation.name}:{sys.version_info[:2]}:{sysconfig.get_platform()}".encode(),
    )
    for name, value in sorted(environment.items()):
        if name.startswith(
            ("CARGO_", "MATURIN_", "PYO3_", "RUST", "SOLDR_")
        ) or name in {
            "AR",
            "CC",
            "CXX",
            "MACOSX_DEPLOYMENT_TARGET",
            "SDKROOT",
            "SOURCE_DATE_EPOCH",
        }:
            _hash_identity_field(hasher, f"environment/{name}", value.encode())
    root = _project_root()
    ignored_directories: set[str] = set()
    try:
        relative_cache_root = (
            (_wheel_cache_root(environment) / "pep517")
            .resolve()
            .relative_to(root.resolve())
        )
        if relative_cache_root.parts:
            ignored_directories.add(relative_cache_root.parts[0])
    except (OSError, ValueError):
        pass
    _hash_metadata_tree(hasher, root, "source", ignored_directories)
    _hash_metadata_directory(hasher, metadata_directory)
    return (
        _wheel_cache_root(environment)
        / "pep517"
        / "wheels"
        / _project_build_identity(environment)
        / kind,
        hasher.hexdigest(),
    )


def _wheel_cache_restore(
    context: "tuple[Path, str] | None", wheel_directory: str
) -> Optional[str]:
    if context is None:
        return None
    directory, fingerprint = context
    try:
        manifest = json.loads((directory / "manifest.json").read_text(encoding="utf-8"))
        filename = manifest["filename"]
        artifact_name = manifest["artifact"]
        artifact = directory / artifact_name
    except (KeyError, OSError, TypeError, ValueError):
        return None
    if (
        manifest.get("schema") != 1
        or manifest.get("fingerprint") != fingerprint
        or not isinstance(filename, str)
        or Path(filename).name != filename
        or not filename.endswith(".whl")
        or not isinstance(artifact_name, str)
        or Path(artifact_name).name != artifact_name
        or not artifact.is_file()
    ):
        return None
    destination = Path(wheel_directory) / filename
    try:
        destination.unlink(missing_ok=True)
        os.link(artifact, destination)
    except OSError:
        try:
            shutil.copy2(artifact, destination)
        except OSError:
            return None
    return filename


def _wheel_cache_store(
    context: "tuple[Path, str] | None", wheel_directory: str, filename: str
) -> None:
    if (
        context is None
        or Path(filename).name != filename
        or not filename.endswith(".whl")
    ):
        return
    source = Path(wheel_directory) / filename
    if not source.is_file():
        return
    directory, fingerprint = context
    artifact_name = f"{fingerprint}.whl"
    artifact = directory / artifact_name
    temporary = directory / f".{artifact_name}.{os.getpid()}.tmp"
    manifest = directory / "manifest.json"
    previous_artifact: Optional[str] = None
    try:
        directory.mkdir(parents=True, exist_ok=True)
        try:
            previous = json.loads(manifest.read_text(encoding="utf-8"))
            candidate = previous.get("artifact")
            if isinstance(candidate, str) and Path(candidate).name == candidate:
                previous_artifact = candidate
        except (OSError, TypeError, ValueError):
            pass
        temporary.unlink(missing_ok=True)
        try:
            os.link(source, temporary)
        except OSError:
            shutil.copy2(source, temporary)
        os.replace(temporary, artifact)
        manifest.write_text(
            json.dumps(
                {
                    "schema": 1,
                    "fingerprint": fingerprint,
                    "filename": filename,
                    "artifact": artifact_name,
                },
                sort_keys=True,
            ),
            encoding="utf-8",
        )
        if previous_artifact and previous_artifact != artifact_name:
            (directory / previous_artifact).unlink(missing_ok=True)
    except OSError:
        temporary.unlink(missing_ok=True)


def _wheel_cache_finish(
    kind: str,
    wheel_directory: str,
    config_settings: Optional[dict],
    metadata_directory: Optional[str],
    filename: str,
) -> str:
    # Re-scan after a successful build: setuptools/fbuild can create or update
    # staged files during packaging, and the next invocation must observe them.
    _wheel_cache_store(
        _wheel_cache_context(kind, config_settings, metadata_directory),
        wheel_directory,
        filename,
    )
    return filename


def _emit_wheel_cache_hit(
    kind: str, config_settings: Optional[dict], elapsed_seconds: float
) -> None:
    env = _prep_env(config_settings, editable=kind == "editable")
    mode = _stats_mode(env)
    if mode == "off":
        return
    label = "editable wheel" if kind == "editable" else "wheel"
    print(
        f"soldr PEP 517: reused cached {label} in {elapsed_seconds:.1f}s | "
        "wheel cache hit",
        file=sys.stderr,
    )
    if mode == "full":
        print(
            "soldr PEP 517 details: "
            + json.dumps(
                {"build_seconds": round(elapsed_seconds, 3), "wheel_cache": "hit"},
                sort_keys=True,
            ),
            file=sys.stderr,
        )


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
    delegated = _delegate_hook(
        "get_requires_for_build_wheel",
        config_settings=config_settings,
        _config_settings=config_settings,
    )
    if delegated is not None:
        return delegated
    return []


def get_requires_for_build_sdist(config_settings: Optional[dict] = None):
    delegated = _delegate_hook(
        "get_requires_for_build_sdist",
        config_settings=config_settings,
        _config_settings=config_settings,
    )
    if delegated is not None:
        return delegated
    return []


def get_requires_for_build_editable(config_settings: Optional[dict] = None):
    delegated = _delegate_hook(
        "get_requires_for_build_editable",
        config_settings=config_settings,
        _config_settings=config_settings,
    )
    if delegated is not None:
        return delegated
    return []


def prepare_metadata_for_build_wheel(
    metadata_directory: str,
    config_settings: Optional[dict] = None,
) -> str:
    delegated = _delegate_hook(
        "prepare_metadata_for_build_wheel",
        metadata_directory,
        config_settings=config_settings,
        _config_settings=config_settings,
    )
    if delegated is not None:
        return delegated
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


def prepare_metadata_for_build_editable(
    metadata_directory: str,
    config_settings: Optional[dict] = None,
) -> str:
    delegated = _delegate_hook(
        "prepare_metadata_for_build_editable",
        metadata_directory,
        fallback="prepare_metadata_for_build_wheel",
        config_settings=config_settings,
        _config_settings=config_settings,
    )
    if delegated is not None:
        return delegated
    return prepare_metadata_for_build_wheel(metadata_directory, config_settings)


def build_wheel(
    wheel_directory: str,
    config_settings: Optional[dict] = None,
    metadata_directory: Optional[str] = None,
) -> str:
    started_at = time.perf_counter()
    cached = _wheel_cache_restore(
        _wheel_cache_context("wheel", config_settings, metadata_directory),
        wheel_directory,
    )
    if cached is not None:
        _emit_wheel_cache_hit(
            "wheel", config_settings, time.perf_counter() - started_at
        )
        return cached
    delegated = _delegate_hook(
        "build_wheel",
        wheel_directory,
        config_settings=config_settings,
        metadata_directory=metadata_directory,
        _config_settings=config_settings,
    )
    if delegated is not None:
        return _wheel_cache_finish(
            "wheel", wheel_directory, config_settings, metadata_directory, delegated
        )
    # maturin pep517 build-wheel does not accept --metadata-directory;
    # the dist-info is regenerated, which PEP 517 explicitly permits.
    target_args = _target_args(config_settings)
    _maturin_pep517(
        "build-wheel",
        "--interpreter",
        sys.executable,
        "--out",
        wheel_directory,
        *_profile_args(config_settings),
        *target_args,
        build_label="wheel",
    )
    return _wheel_cache_finish(
        "wheel",
        wheel_directory,
        config_settings,
        metadata_directory,
        _newest_entry(wheel_directory, ".whl", want_dir=False),
    )


def build_editable(
    wheel_directory: str,
    config_settings: Optional[dict] = None,
    metadata_directory: Optional[str] = None,
) -> str:
    started_at = time.perf_counter()
    cached = _wheel_cache_restore(
        _wheel_cache_context("editable", config_settings, metadata_directory),
        wheel_directory,
    )
    if cached is not None:
        _emit_wheel_cache_hit(
            "editable", config_settings, time.perf_counter() - started_at
        )
        return cached
    delegated = _delegate_hook(
        "build_editable",
        wheel_directory,
        config_settings=config_settings,
        metadata_directory=metadata_directory,
        _config_settings=config_settings,
    )
    if delegated is not None:
        return _wheel_cache_finish(
            "editable", wheel_directory, config_settings, metadata_directory, delegated
        )
    target_args = _target_args(config_settings)
    _maturin_pep517(
        "build-wheel",
        "--interpreter",
        sys.executable,
        "--out",
        wheel_directory,
        "--editable",
        *_profile_args(config_settings, editable=True),
        *target_args,
        build_label="editable wheel",
    )
    return _wheel_cache_finish(
        "editable",
        wheel_directory,
        config_settings,
        metadata_directory,
        _newest_entry(wheel_directory, ".whl", want_dir=False),
    )


def build_sdist(
    sdist_directory: str,
    config_settings: Optional[dict] = None,
) -> str:
    delegated = _delegate_hook(
        "build_sdist",
        sdist_directory,
        config_settings=config_settings,
        _config_settings=config_settings,
    )
    if delegated is not None:
        return delegated
    _maturin_pep517(
        "write-sdist",
        "--sdist-directory",
        sdist_directory,
    )
    return _newest_entry(sdist_directory, ".tar.gz", want_dir=False)
