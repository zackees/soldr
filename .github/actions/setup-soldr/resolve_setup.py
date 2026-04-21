#!/usr/bin/env python3
from __future__ import annotations

import hashlib
import json
import os
import re
from pathlib import Path
from typing import Any

try:
    import tomllib
except ModuleNotFoundError:  # pragma: no cover
    tomllib = None


def _normalize_list(value: Any) -> list[str]:
    if not value:
        return []
    if isinstance(value, str):
        return [part.strip() for part in value.split(",") if part.strip()]
    return [str(item).strip() for item in value if str(item).strip()]


def load_toolchain_spec(
    workspace: Path,
    toolchain_file: str,
    toolchain_override: str,
) -> dict[str, Any]:
    channel = "stable"
    profile = "minimal"
    components: list[str] = []
    targets: list[str] = []
    source = "default"
    file_hash = "none"

    if toolchain_file:
        path = workspace / toolchain_file
        if path.exists():
            source = str(path.relative_to(workspace))
            file_bytes = path.read_bytes()
            file_hash = hashlib.sha256(file_bytes).hexdigest()[:16]
            if tomllib is None:
                raise RuntimeError("python tomllib support is required for setup-soldr")
            data = tomllib.loads(file_bytes.decode("utf-8"))
            toolchain = data.get("toolchain", {})
            if isinstance(toolchain, dict):
                channel = str(toolchain.get("channel", channel))
                profile = str(toolchain.get("profile", profile))
                components = _normalize_list(toolchain.get("components"))
                targets = _normalize_list(toolchain.get("targets"))

    if toolchain_override:
        channel = toolchain_override.strip()
        source = "input"

    return {
        "channel": channel,
        "profile": profile,
        "components": components,
        "targets": targets,
        "source": source,
        "file_hash": file_hash,
    }


def _sanitize_fragment(value: str) -> str:
    return re.sub(r"[^A-Za-z0-9._-]+", "-", value).strip("-") or "default"


def _short_file_hash(path: Path, missing: str) -> str:
    if not path.exists():
        return missing
    return hashlib.sha256(path.read_bytes()).hexdigest()[:16]


def _write_env(name: str, value: str) -> None:
    output = os.environ.get("GITHUB_ENV")
    if not output:
        return
    with open(output, "a", encoding="utf-8") as fh:
        fh.write(f"{name}={value}\n")


def _write_path(value: str) -> None:
    output = os.environ.get("GITHUB_PATH")
    if not output:
        return
    with open(output, "a", encoding="utf-8") as fh:
        fh.write(f"{value}\n")


def _write_outputs(values: dict[str, str]) -> None:
    output = os.environ.get("GITHUB_OUTPUT")
    if not output:
        return
    with open(output, "a", encoding="utf-8") as fh:
        for key, value in values.items():
            if "\n" in value:
                # Use GitHub's heredoc delimiter form for multi-line outputs.
                delimiter = f"ghadelim_{hashlib.sha256(value.encode()).hexdigest()[:16]}"
                fh.write(f"{key}<<{delimiter}\n{value}\n{delimiter}\n")
            else:
                fh.write(f"{key}={value}\n")


def main() -> None:
    workspace = Path(os.environ["ACTION_WORKSPACE"]).resolve()
    runner_temp = Path(os.environ.get("RUNNER_TEMP", workspace / ".tmp")).resolve()

    requested_cache_dir = os.environ.get("INPUT_CACHE_DIR", "").strip()
    cache_root = Path(requested_cache_dir).expanduser().resolve() if requested_cache_dir else (
        runner_temp / "setup-soldr"
    )
    soldr_root = cache_root / "soldr"
    cargo_home = cache_root / "cargo"
    rustup_home = cache_root / "rustup"
    bin_dir = cache_root / "bin"
    soldr_binary = "soldr.exe" if os.name == "nt" else "soldr"
    soldr_path = bin_dir / soldr_binary

    for path in (
        cache_root,
        soldr_root,
        soldr_root / "cache",
        soldr_root / "bin",
        cargo_home,
        cargo_home / "bin",
        rustup_home,
        bin_dir,
    ):
        path.mkdir(parents=True, exist_ok=True)

    toolchain = load_toolchain_spec(
        workspace=workspace,
        toolchain_file=os.environ.get("INPUT_TOOLCHAIN_FILE", "rust-toolchain.toml"),
        toolchain_override=os.environ.get("INPUT_TOOLCHAIN", ""),
    )

    soldr_repo = os.environ.get("INPUT_REPO", "zackees/soldr").strip() or "zackees/soldr"
    soldr_version = os.environ.get("INPUT_VERSION", "").strip()
    toolchain_signature = {
        "channel": toolchain["channel"],
        "profile": toolchain["profile"],
        "components": toolchain["components"],
        "targets": toolchain["targets"],
        "source": toolchain["source"],
        "file_hash": toolchain["file_hash"],
        "soldr_repo": soldr_repo,
        "soldr_version": soldr_version or "latest",
    }
    digest = hashlib.sha256(
        json.dumps(toolchain_signature, sort_keys=True).encode("utf-8")
    ).hexdigest()[:16]
    runner_os = _sanitize_fragment(os.environ.get("ACTION_OS", os.name).lower())
    runner_arch = _sanitize_fragment(os.environ.get("ACTION_ARCH", "unknown").lower())
    cache_prefix = f"setup-soldr-v1-{runner_os}-{runner_arch}"
    cache_key = f"{cache_prefix}-{digest}"
    cargo_lock_hash = _short_file_hash(workspace / "Cargo.lock", "no-lock")

    suffix = os.environ.get("INPUT_CACHE_KEY_SUFFIX", "").strip()
    if suffix:
        cache_key = f"{cache_key}-{_sanitize_fragment(suffix)}"

    # Build-artifact cache (zccache compilation cache at ~/.zccache).
    # Key shape: setup-soldr-buildcache-v1-{os}-{arch}-{toolchain-digest}-{sha}.
    # Restore falls back through the same toolchain lineage and then any
    # OS+arch cache, letting GitHub's own-branch -> PR base -> default branch
    # restore order provide parent -> child lineage without user config.
    github_sha = os.environ.get("GITHUB_SHA", "").strip() or "nosha"
    build_cache_prefix = f"setup-soldr-buildcache-v1-{runner_os}-{runner_arch}"
    build_cache_toolchain_prefix = f"{build_cache_prefix}-{digest}-"
    build_cache_key = f"{build_cache_toolchain_prefix}{github_sha}"

    target_dir_input = os.environ.get("INPUT_TARGET_DIR", "target").strip() or "target"
    target_cache_path = Path(target_dir_input).expanduser()
    if not target_cache_path.is_absolute():
        target_cache_path = workspace / target_cache_path
    target_cache_path = target_cache_path.resolve()
    target_cache_prefix = f"setup-soldr-targetcache-v1-{runner_os}-{runner_arch}"
    target_cache_lock_prefix = f"{target_cache_prefix}-{digest}-{cargo_lock_hash}-"
    target_cache_key = f"{target_cache_lock_prefix}{github_sha}"

    if suffix:
        sanitized_suffix = _sanitize_fragment(suffix)
        build_cache_key = f"{build_cache_key}-{sanitized_suffix}"
        target_cache_key = f"{target_cache_key}-{sanitized_suffix}"

    _write_env("SOLDR_CACHE_DIR", str(soldr_root))
    _write_env("CARGO_HOME", str(cargo_home))
    _write_env("RUSTUP_HOME", str(rustup_home))
    _write_env("SETUP_SOLDR_TOOLCHAIN_CHANNEL", toolchain["channel"])
    _write_env("SETUP_SOLDR_TOOLCHAIN_PROFILE", toolchain["profile"])
    _write_env("SETUP_SOLDR_TOOLCHAIN_COMPONENTS", json.dumps(toolchain["components"]))
    _write_env("SETUP_SOLDR_TOOLCHAIN_TARGETS", json.dumps(toolchain["targets"]))
    if os.environ.get("INPUT_TRUST_MODE", "").strip():
        _write_env("SOLDR_TRUST_MODE", os.environ["INPUT_TRUST_MODE"].strip())

    _write_path(str(bin_dir))
    _write_path(str(cargo_home / "bin"))

    _write_outputs(
        {
            "cache_root": str(cache_root),
            "cache_key": cache_key,
            "cache_restore_prefix": f"{cache_prefix}-",
            "build_cache_key": build_cache_key,
            "build_cache_restore_key_toolchain": build_cache_toolchain_prefix,
            "build_cache_restore_key_os_arch": f"{build_cache_prefix}-",
            "target_cache_path": str(target_cache_path),
            "target_cache_key": target_cache_key,
            "target_cache_restore_key_lock": target_cache_lock_prefix,
            "soldr_root": str(soldr_root),
            "cargo_home": str(cargo_home),
            "rustup_home": str(rustup_home),
            "bin_dir": str(bin_dir),
            "soldr_path": str(soldr_path),
            "soldr_repo": soldr_repo,
            "soldr_version_requested": soldr_version,
            "toolchain_channel": toolchain["channel"],
            "toolchain_profile": toolchain["profile"],
            "toolchain_source": toolchain["source"],
            "toolchain": toolchain["channel"],
        }
    )


if __name__ == "__main__":
    main()
