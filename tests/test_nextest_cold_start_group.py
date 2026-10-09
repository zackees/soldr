"""soldr#3625: every cold broker/daemon start fixture shares one capped group.

A Soldr front door handed a cacheable compile on a fresh ``SOLDR_CACHE_DIR``
pays a broker spawn, route acquisition and daemon bring-up before its first
compile is answered. Four of those overlapping on a loaded nextest run starved
the broker's progress-silence deadline (soldr#3625), so ``.config/nextest.toml``
caps them with ``soldr-cold-start-linux`` on Linux and the one-thread
``soldr-runtime`` / ``soldr-cargo-cold-builds`` groups elsewhere.

nextest selects group members by module name, and a module nobody adds to the
filter is silently uncapped. This suite recognises the *shape* in the test
sources instead of trusting the list: every module with the shape must be named
by a cold-start group on every platform, and every name in those filters must
still be a real module. The direction is deliberate -- the groups also carry
hand-justified members (direct broker/daemon lifecycle tests) that this
front-door predicate does not describe, so membership beyond the shape is
allowed; a missing member is not.
"""

from __future__ import annotations

import re
import tomllib
from dataclasses import dataclass
from itertools import pairwise
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
CONFIG = REPO_ROOT / ".config" / "nextest.toml"
CLI_TESTS = REPO_ROOT / "crates" / "soldr-cli" / "tests"

LINUX_GROUP = "soldr-cold-start-linux"
OFF_LINUX_GROUPS = frozenset({"soldr-runtime", "soldr-cargo-cold-builds"})
LINUX_PLATFORM = 'cfg(target_os = "linux")'

# A Soldr process: the scrubbed fixture helper (and its `_in` / `_with_target`
# variants) or a raw `Command::new(soldr_bin())`.
_FRONT_DOOR = re.compile(r"\bisolated_soldr_command\w*\(|\bsoldr_bin\(\)")
# A literal argv: `["cargo", "build", ...]` or a single `.arg("rust-analyzer")`.
_ARGV_ARRAY = re.compile(r'\[\s*((?:"[^"\n]*"\s*,\s*)*"[^"\n]*")\s*,?\s*\]')
_SINGLE_ARG = re.compile(r'\.arg\("([^"\n]+)"\)')
_STRING = re.compile(r'"([^"\n]*)"')
# Cargo verbs that invoke rustc, so the compile reaches Soldr's RUSTC_WRAPPER
# shim and with it the broker's SESSION route.
_COMPILE_VERBS = frozenset(
    {"build", "check", "test", "clippy", "doc", "run", "dylint", "fmt", "bench"}
)
# Soldr verbs that run a nested cargo through the same route.
_NESTED_CARGO_VERBS = frozenset({"maturin", "wheel", "rust-analyzer"})
_MOD_DECL = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;", re.MULTILINE)
# The module a nextest filter term selects: `test(/^(a|b)::/)`,
# `test(/^a::(x|y)$/)` or `test(=a::x)`.
_FILTER_MODULES = re.compile(r"test\((?:=|/\^\(?)([A-Za-z0-9_|]+?)\)?::")
_FILTER_BINARY_MODULES = re.compile(
    r"binary\(([A-Za-z0-9_]+)\) & test\(/\^\(?([A-Za-z0-9_|]+?)\)?::"
)


def routes_a_cached_compile(argv: list[str]) -> bool:
    """True when `argv` sends a compile through the broker with the cache on."""

    if "--no-cache" in argv:
        return False
    for verb, sub in pairwise(argv):
        if verb == "cargo" and sub in _COMPILE_VERBS:
            return True
        if verb == "exec" and sub == "cargo":
            return True
    return bool(argv) and argv[0] in _NESTED_CARGO_VERBS


def has_cold_start_shape(source: str) -> bool:
    """A module that launches Soldr with at least one cached, routed compile."""

    if not _FRONT_DOOR.search(source):
        return False
    argvs = [_STRING.findall(match.group(1)) for match in _ARGV_ARRAY.finditer(source)]
    argvs += [[match.group(1)] for match in _SINGLE_ARG.finditer(source)]
    return any(routes_a_cached_compile(argv) for argv in argvs)


@dataclass(frozen=True)
class ModuleRef:
    """One soldr-cli test module: its name, category binary and source file."""

    name: str
    binary: str
    path: Path


@dataclass(frozen=True)
class GroupOverride:
    """One `[[profile.default.overrides]]` entry that assigns a test group."""

    index: int
    filter: str
    group: str
    platform_gated: bool
    linux: bool


def declared_modules() -> list[ModuleRef]:
    """Every soldr-cli test module declared by a category binary's main.rs."""

    modules: list[ModuleRef] = []
    for main in sorted(CLI_TESTS.glob("*/main.rs")):
        category = main.parent.name
        for name in _MOD_DECL.findall(main.read_text(encoding="utf-8")):
            if name == "common":
                continue
            path = main.parent / f"{name}.rs"
            if not path.is_file():
                path = main.parent / name / "mod.rs"
            assert path.is_file(), (
                f"{category}/main.rs declares `mod {name};` with no source"
            )
            modules.append(ModuleRef(name, category, path))
    return modules


def cold_start_modules() -> list[ModuleRef]:
    """Every module with the cold-start shape."""

    return [
        module
        for module in declared_modules()
        if has_cold_start_shape(module.path.read_text(encoding="utf-8"))
    ]


def _group_overrides() -> list[GroupOverride]:
    with CONFIG.open("rb") as handle:
        config = tomllib.load(handle)
    overrides = [
        override
        for override in config["profile"]["default"]["overrides"]
        if "test-group" in override
    ]
    return [
        GroupOverride(
            index=index,
            filter=override["filter"],
            group=override["test-group"],
            platform_gated="platform" in override,
            linux=override.get("platform", {}).get("target") == LINUX_PLATFORM,
        )
        for index, override in enumerate(overrides)
    ]


def _named_modules(filter_expr: str) -> set[str]:
    named: set[str] = set()
    for match in _FILTER_MODULES.finditer(filter_expr):
        named.update(match.group(1).split("|"))
    return named


def test_the_shape_is_recognised_in_every_recorded_soldr_3625_failure() -> None:
    """The predicate must see the modules that actually failed, or it is blind."""

    detected = {module.name for module in cold_start_modules()}
    for failed in (
        "daemon_restart_warmth",
        "cli_cargo_zccache_mode",
        "cli_maturin",
        # The third failure's other contender.
        "cli_rust_analyzer",
    ):
        assert failed in detected, sorted(detected)


def test_the_argv_classifier_separates_routed_compiles_from_cache_off_ones() -> None:
    assert routes_a_cached_compile(["cargo", "build"])
    assert routes_a_cached_compile(["--debug", "cargo", "check"])
    assert routes_a_cached_compile(["exec", "cargo", "build"])
    assert routes_a_cached_compile(["maturin", "build"])
    assert routes_a_cached_compile(["rust-analyzer"])
    # The cache kill-switch never reaches the broker's SESSION route.
    assert not routes_a_cached_compile(["--no-cache", "cargo", "build"])
    # Commands that compile nothing.
    assert not routes_a_cached_compile(["cargo", "metadata"])
    assert not routes_a_cached_compile(["status", "--json"])
    assert not routes_a_cached_compile(["toolchain", "prepare"])
    assert not has_cold_start_shape('Command::new("cargo").args(["cargo", "build"])')


def test_every_cold_start_module_is_capped_on_every_platform() -> None:
    overrides = _group_overrides()
    linux_named: set[str] = set()
    off_linux_named: set[str] = set()
    for override in overrides:
        named = _named_modules(override.filter)
        if override.group == LINUX_GROUP and override.linux:
            linux_named |= named
        elif override.group in OFF_LINUX_GROUPS and not override.platform_gated:
            off_linux_named |= named
    shape = {module.name for module in cold_start_modules()}
    missing_linux = sorted(set(shape) - linux_named)
    missing_elsewhere = sorted(set(shape) - off_linux_named)
    assert not missing_linux and not missing_elsewhere, (
        "test modules that hand a Soldr front door a cached compile on a fresh "
        "root cold-start a broker and daemon, and must share the cold-start "
        "group or they overlap other cold starts and trip the broker's "
        "progress-silence deadline (soldr#3625). Add them to the "
        f"`{LINUX_GROUP}` override and its ungated twin in .config/nextest.toml. "
        f"Missing on Linux: {missing_linux}; missing elsewhere: {missing_elsewhere}"
    )


def test_cold_start_group_filters_name_only_real_modules() -> None:
    """A renamed module leaves a filter term that nextest silently ignores."""

    modules = declared_modules()
    for override in _group_overrides():
        for name in _named_modules(override.filter):
            assert any(module.name == name for module in modules), (
                f"`{name}` in test-group `{override.group}` is not a "
                "soldr-cli test module; nextest ignores the term and the tests "
                f"it meant run uncapped.\nfilter = {override.filter}"
            )
        for binary, alternation in _FILTER_BINARY_MODULES.findall(override.filter):
            for name in alternation.split("|"):
                homes = {module.binary for module in modules if module.name == name}
                assert homes == {binary}, f"`{name}` lives in {homes}, not `{binary}`"


def test_the_linux_group_mirrors_the_off_linux_groups() -> None:
    """Same members on every platform; the Linux twin must win first-match."""

    with CONFIG.open("rb") as handle:
        groups = tomllib.load(handle)["test-groups"]
    assert groups[LINUX_GROUP]["max-threads"] == 2
    for group in OFF_LINUX_GROUPS:
        assert groups[group]["max-threads"] == 1, group
    # soldr#3625: one Linux group. Separate Linux twins run concurrently and
    # re-create the four-wide overlap this file exists to prevent.
    assert sorted(groups) == sorted({LINUX_GROUP, *OFF_LINUX_GROUPS}), groups

    overrides = _group_overrides()
    linux = [override for override in overrides if override.linux]
    elsewhere = [override for override in overrides if not override.platform_gated]
    assert all(override.group == LINUX_GROUP for override in linux)
    assert all(override.group in OFF_LINUX_GROUPS for override in elsewhere)
    assert {o.filter for o in linux} == {o.filter for o in elsewhere}, (
        "every cold-start filter needs a Linux-gated twin with identical text"
    )
    for twin in linux:
        # nextest takes a setting from the FIRST matching override, so a twin
        # listed after its ungated sibling would never apply.
        sibling = next(o for o in elsewhere if o.filter == twin.filter)
        assert twin.index < sibling.index, twin.filter
