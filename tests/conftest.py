"""Pytest collection hooks shared across the soldr test suite."""

from __future__ import annotations

import importlib.util
import re
import shutil
import subprocess
import sys
from pathlib import Path
from types import ModuleType

import pytest


def load_script_module(path: str | Path, name: str | None = None) -> ModuleType:
    """Import a standalone script as a module (soldr#2113).

    The scripts these guards cover live in `.github/scripts/` and similar
    non-package directories, so there is no `import` statement that reaches
    them. Every guard hand-rolled the same importlib dance; this is that
    dance, once.

    `name` defaults to the file stem. The module is registered in
    `sys.modules` *before* `exec_module` runs, which several of the guarded
    scripts require: a dataclass resolves its own `__module__` through
    `sys.modules` while the class is being created, and raises `KeyError` if
    the entry is not there yet. Registering unconditionally is the superset of
    what the call sites did, so no caller loses behaviour by moving here.
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


# Release scripts are standalone modules, but several share this module. Load it
# once during test collection so their direct sibling import resolves under the
# importlib-based test loader too.
RELEASE_ARTIFACTS = load_script_module(
    Path(__file__).parents[1] / ".github" / "scripts" / "release_artifacts.py",
    "release_artifacts",
)


# The workspace's first-party crates. Two guards assert over this same list,
# and a crate added to the workspace must reach both of them.
WORKSPACE_CRATES = [
    "soldr-cli",
    "soldr-core",
    "soldr-fetch",
    "soldr-cache",
    "soldr-daemon",
]

# The triples soldr ships, spelled out rather than derived from
# `ci/canonical-targets.json`. Two guards assert over this same list -- the
# target-removal contract and the Dylint driver-asset check -- so a contract
# edit that drops one has to answer to both, instead of quietly shrinking the
# set each of them checks. Dropping a target is a reviewed decision
# (soldr#2469 step 2.1): it needs a `compatibility_decisions` entry and this
# list updated in the same PR.
RELEASE_INCLUDED_TRIPLES = [
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
]

# Canonical Dylint CI policy shared by the nightly-agreement and process-
# boundary guards. The full rustup toolchain key is also the target-directory
# key, keeping nightly artifacts separate from the project's Rust 1.95 tree.
DYLINT_NIGHTLY = "nightly-2026-05-28-x86_64-unknown-linux-gnu"
DYLINT_BUILD_STEPS = (
    "Build daemon process-creation boundary lint",
    "Build fetch network boundary lint",
    "Build local-socket name boundary lint",
    "Build raw IPC transport boundary lint",
    "Build platform-cfg directory boundary lint",
    "Build env-flag boundary lint",
)
DYLINT_TEST_STEPS = (
    "Test env-flag boundary lint",
    "Test daemon process-creation boundary lint",
    "Test fetch network boundary lint",
    "Test local-socket name boundary lint",
    "Test raw IPC transport boundary lint",
    "Test platform-cfg directory boundary lint",
)


def workflow_step(workflow: str, name: str) -> str:
    """Return one named GitHub Actions step body from a workflow string."""

    match = re.search(
        rf"^      - name: {re.escape(name)}\n(?P<body>.*?)(?=^      - name: |\Z)",
        workflow,
        re.MULTILINE | re.DOTALL,
    )
    if match is None:
        raise AssertionError(f"workflow has no {name!r} step")
    return match.group("body")


def docker_available() -> bool:
    """Whether a reachable docker daemon exists.

    Both docker-backed guards are opt-in and skip without one. `docker info`
    is the probe rather than `which docker`, because a present client with a
    dead daemon is the common local state (see the Docker Desktop notes in
    CLAUDE.md).

    The two call sites had different timeouts (10s and 20s); this takes the
    larger. A daemon that answers in 15s would otherwise be reported absent,
    silently downgrading a real integration run to a skip.
    """

    if shutil.which("docker") is None:
        return False
    try:
        result = subprocess.run(
            ["docker", "info"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=20,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return False
    return result.returncode == 0


def git(repo: Path, *args: str) -> str:
    """Run git in `repo` and return stdout, raising on failure."""

    return subprocess.run(
        ["git", *args], cwd=repo, check=True, capture_output=True, text=True
    ).stdout


@pytest.fixture
def repo(tmp_path: Path) -> Path:
    """A throwaway git repo with an identity configured.

    Commits fail outright without `user.email` / `user.name`, and CI runners
    have no global git identity, so configuring it is part of the fixture
    rather than something each test remembers.
    """

    root = tmp_path / "repo"
    root.mkdir()
    git(root, "init", "-q", "-b", "main")
    git(root, "config", "user.email", "t@example.com")
    git(root, "config", "user.name", "t")
    return root


def pytest_addoption(parser: pytest.Parser) -> None:
    parser.addoption(
        "--act-integration",
        action="store_true",
        default=False,
        help=(
            "Run docker-based act-image integration tests (marked "
            "@pytest.mark.act_integration). Skipped by default because they "
            "require docker + ~1 GB image pulls."
        ),
    )
    parser.addoption(
        "--cacheability-integration",
        action="store_true",
        default=False,
        help=(
            "Run Docker-based cacheability integration tests (marked "
            "@pytest.mark.cacheability_integration). Skipped by default "
            "because they build the full nextest archive twice."
        ),
    )


def pytest_collection_modifyitems(
    config: pytest.Config, items: list[pytest.Item]
) -> None:
    marker_options = {
        "act_integration": "--act-integration",
        "cacheability_integration": "--cacheability-integration",
    }
    marker_expression = config.getoption("-m") or ""
    for item in items:
        for marker, option in marker_options.items():
            if marker not in item.keywords:
                continue
            if config.getoption(option) or marker in marker_expression:
                continue
            item.add_marker(
                pytest.mark.skip(
                    reason=(
                        f"{marker} tests are opt-in. Re-run with {option} "
                        f"or `-m {marker}` to execute them."
                    )
                )
            )
