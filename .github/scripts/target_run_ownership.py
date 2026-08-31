#!/usr/bin/env python3
"""Build and verify Soldr's positive native target-run selection.

The nextest archive remains complete.  This helper decides which tests execute
on a target-native worker from ``ci/target-run-ownership.json`` and refuses to
produce a filter when an owner is stale or the union is empty.  Its source
guard is the inverse half of the contract: integration modules that exercise
real host facilities must be covered by an owner, so a newly added native test
cannot silently remain Linux-only.
"""

from __future__ import annotations

import argparse
import json
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Any


SCHEMA_VERSION = 1
SAFE_NAME = re.compile(r"^[A-Za-z0-9_-]+$")
SAFE_PREFIX = re.compile(r"^[A-Za-z0-9_:]+$")
SAFE_TARGET = re.compile(r"^[A-Za-z0-9_.-]+$")
MODULE_DECLARATION = re.compile(r"(?m)^\s*(?:pub\s+)?mod\s+([A-Za-z0-9_]+)\s*;")
HOST_CFG = re.compile(
    r"cfg(?:_attr)?\s*\([^\n]*(?:windows|unix|target_os|target_arch|target_env)"
)

# The inverse guard deliberately uses semantic host boundaries, not merely
# ``cfg`` attributes. Soldr keeps host selection inside soldr-platform, so
# most integration contracts are runtime-gated and would be missed by a
# cfg-only scan.
HOST_MARKERS = (
    "std::process::",
    "std::fs::",
    "std::os::",
    "Command::new(",
    "tempfile::",
    "tokio::process::",
    "running_process::",
    "soldr_platform::fs::",
    "soldr_platform::host::",
    "soldr_platform::ipc::",
    "soldr_platform::process::",
)


@dataclass(frozen=True)
class Owner:
    id: str
    package: str
    binary: str
    reason: str
    test_prefix: str | None = None
    targets: tuple[str, ...] | None = None

    def applies_to(self, target: str) -> bool:
        return self.targets is None or target in self.targets

    def matches(self, package: str, binary: str, test_name: str) -> bool:
        return (
            self.package == package
            and self.binary == binary
            and (self.test_prefix is None or test_name.startswith(self.test_prefix))
        )

    def filter_expression(self) -> str:
        expression = f"package({self.package}) & binary({self.binary})"
        if self.test_prefix is not None:
            expression += f" & test(/^{self.test_prefix}/)"
        return f"({expression})"


@dataclass(frozen=True)
class Selection:
    target: str
    discovered_count: int
    selected_count: int
    test_ids: tuple[str, ...]
    filter_expression: str
    owner_counts: tuple[tuple[str, int], ...]


def _require_nonempty_string(value: Any, label: str) -> str:
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f"{label} must be a non-empty string")
    return value


def parse_manifest(payload: dict[str, object]) -> tuple[Owner, ...]:
    if payload.get("schema_version") != SCHEMA_VERSION:
        raise ValueError(
            f"target-run ownership schema_version must be {SCHEMA_VERSION}"
        )
    if payload.get("policy_issue") != "soldr#2999":
        raise ValueError("target-run ownership policy_issue must be soldr#2999")
    raw_owners = payload.get("owners")
    if not isinstance(raw_owners, list):
        raise ValueError("target-run ownership owners must be an array")

    owners: list[Owner] = []
    seen_ids: set[str] = set()
    for index, raw_owner in enumerate(raw_owners):
        label = f"owners[{index}]"
        if not isinstance(raw_owner, dict):
            raise ValueError(f"{label} must be an object")
        allowed = {"id", "package", "binary", "test_prefix", "targets", "reason"}
        extra = set(raw_owner) - allowed
        missing = {"id", "package", "binary", "reason"} - set(raw_owner)
        if extra or missing:
            raise ValueError(
                f"{label} keys disagree: missing={sorted(missing)} extra={sorted(extra)}"
            )
        owner_id = _require_nonempty_string(raw_owner["id"], f"{label}.id")
        package = _require_nonempty_string(raw_owner["package"], f"{label}.package")
        binary = _require_nonempty_string(raw_owner["binary"], f"{label}.binary")
        reason = _require_nonempty_string(raw_owner["reason"], f"{label}.reason")
        prefix_value = raw_owner.get("test_prefix")
        test_prefix = (
            None
            if prefix_value is None
            else _require_nonempty_string(prefix_value, f"{label}.test_prefix")
        )
        raw_targets = raw_owner.get("targets")
        targets: tuple[str, ...] | None
        if raw_targets is None:
            targets = None
        elif not isinstance(raw_targets, list) or not raw_targets:
            raise ValueError(f"{label}.targets must be a non-empty array")
        else:
            parsed_targets = tuple(
                _require_nonempty_string(value, f"{label}.targets[{target_index}]")
                for target_index, value in enumerate(raw_targets)
            )
            if any(not SAFE_TARGET.fullmatch(target) for target in parsed_targets):
                raise ValueError(f"{label}.targets contains an unsafe target triple")
            if len(set(parsed_targets)) != len(parsed_targets):
                raise ValueError(f"{label}.targets contains duplicates")
            targets = parsed_targets
        if not SAFE_NAME.fullmatch(package) or not SAFE_NAME.fullmatch(binary):
            raise ValueError(
                f"{label} package/binary contains unsafe filter characters"
            )
        if test_prefix is not None and not SAFE_PREFIX.fullmatch(test_prefix):
            raise ValueError(f"{label}.test_prefix contains unsafe filter characters")
        if owner_id in seen_ids:
            raise ValueError(f"duplicate target-run owner id: {owner_id}")
        seen_ids.add(owner_id)
        owners.append(Owner(owner_id, package, binary, reason, test_prefix, targets))
    return tuple(owners)


def load_manifest(path: Path) -> dict[str, object]:
    if not path.is_file():
        raise ValueError(f"target-run ownership manifest is missing: {path}")
    payload = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(payload, dict):
        raise ValueError("target-run ownership manifest must be a JSON object")
    parse_manifest(payload)
    return payload


def _inventory_tests(payload: dict[str, object]) -> tuple[tuple[str, str, str], ...]:
    suites = payload.get("rust-suites")
    if not isinstance(suites, dict):
        raise ValueError("nextest list JSON is missing rust-suites object")
    tests: list[tuple[str, str, str]] = []
    for suite_id, raw_suite in suites.items():
        if not isinstance(raw_suite, dict):
            raise ValueError(f"nextest suite {suite_id!r} must be an object")
        package = raw_suite.get("package-name")
        binary = raw_suite.get("binary-name")
        testcases = raw_suite.get("testcases")
        if not isinstance(package, str) or not isinstance(binary, str):
            raise ValueError(
                f"nextest suite {suite_id!r} is missing package-name/binary-name"
            )
        if not isinstance(testcases, dict):
            raise ValueError(f"nextest suite {suite_id!r} is missing testcases")
        tests.extend(
            (package, binary, name) for name in testcases if isinstance(name, str)
        )
    declared_count = payload.get("test-count")
    if not isinstance(declared_count, int) or isinstance(declared_count, bool):
        raise ValueError("nextest list JSON is missing integer test-count")
    if declared_count != len(tests):
        raise ValueError(
            "nextest list test-count disagrees with suites: "
            f"declared={declared_count} observed={len(tests)}"
        )
    return tuple(sorted(tests))


def build_selection(
    manifest: dict[str, object], inventory: dict[str, object], target: str
) -> Selection:
    owners = tuple(
        owner for owner in parse_manifest(manifest) if owner.applies_to(target)
    )
    tests = _inventory_tests(inventory)
    selected: set[tuple[str, str, str]] = set()
    owner_counts: list[tuple[str, int]] = []
    for owner in owners:
        matches = {test for test in tests if owner.matches(*test)}
        if not matches:
            raise ValueError(
                f"target-run owner {owner.id!r} matched no tests for target {target}; "
                "the selector is stale or the archive lost required native coverage"
            )
        overlap = selected.intersection(matches)
        if overlap:
            example = "::".join(sorted(overlap)[0])
            raise ValueError(
                f"target-run owner {owner.id!r} overlaps an earlier owner at {example}"
            )
        selected.update(matches)
        owner_counts.append((owner.id, len(matches)))
    if not selected:
        raise ValueError(f"target-run ownership selects zero tests for target {target}")

    test_ids = tuple("::".join(test) for test in sorted(selected))
    expression = " | ".join(owner.filter_expression() for owner in owners)
    return Selection(
        target=target,
        discovered_count=len(tests),
        selected_count=len(selected),
        test_ids=test_ids,
        filter_expression=expression,
        owner_counts=tuple(owner_counts),
    )


def _is_host_sensitive(source: str) -> bool:
    return HOST_CFG.search(source) is not None or any(
        marker in source for marker in HOST_MARKERS
    )


def _owned_by(
    owners: tuple[Owner, ...], package: str, binary: str, prefix: str
) -> bool:
    return any(
        owner.package == package
        and owner.binary == binary
        and (owner.test_prefix is None or prefix.startswith(owner.test_prefix))
        for owner in owners
    )


def validate_source_ownership(manifest: dict[str, object], repo_root: Path) -> None:
    """Fail when host-sensitive integration source has no positive owner."""

    owners = parse_manifest(manifest)
    crates_root = repo_root / "crates"
    failures: list[str] = []
    observed_binaries: set[tuple[str, str]] = set()

    if crates_root.is_dir():
        for crate in sorted(path for path in crates_root.iterdir() if path.is_dir()):
            package = crate.name
            tests_root = crate / "tests"
            if not tests_root.is_dir():
                continue

            for main_path in sorted(tests_root.glob("*/main.rs")):
                binary = main_path.parent.name
                observed_binaries.add((package, binary))
                main_source = main_path.read_text(encoding="utf-8")
                for module in MODULE_DECLARATION.findall(main_source):
                    # All category binaries share fixture helpers through a
                    # #[path] declaration. It is not a test module and has no
                    # independently selectable nextest prefix.
                    if module == "common":
                        continue
                    module_path = main_path.parent / f"{module}.rs"
                    if not module_path.is_file():
                        failures.append(
                            f"{main_path.relative_to(repo_root)} declares missing {module}.rs"
                        )
                        continue
                    source = module_path.read_text(encoding="utf-8")
                    # The guards category reads repository files in order to
                    # enforce portable source policy. Filesystem APIs there
                    # are an implementation detail, not host behavior; those
                    # tests deliberately run once in canonical Linux CI.
                    if binary == "guards":
                        continue
                    if _is_host_sensitive(source) and not _owned_by(
                        owners, package, binary, f"{module}::"
                    ):
                        failures.append(
                            "unowned host-sensitive test source: "
                            f"{module_path.relative_to(repo_root)} "
                            f"(expected package={package} binary={binary} "
                            f"test_prefix={module}:: or whole binary)"
                        )

            for source_path in sorted(tests_root.glob("*.rs")):
                binary = source_path.stem
                observed_binaries.add((package, binary))
                source = source_path.read_text(encoding="utf-8")
                if _is_host_sensitive(source) and not _owned_by(
                    owners, package, binary, ""
                ):
                    failures.append(
                        "unowned host-sensitive test source: "
                        f"{source_path.relative_to(repo_root)} "
                        f"(expected package={package} binary={binary})"
                    )

    # soldr-platform is the exclusive host-cfg boundary. Its library tests
    # must stay in target replay even when individual source files use a
    # platform module selected by lib.rs rather than spelling cfg themselves.
    if (repo_root / "crates" / "soldr-platform").is_dir() and not any(
        owner.package == "soldr-platform" and owner.binary == "soldr_platform"
        for owner in owners
    ):
        failures.append(
            "unowned host-sensitive test source: crates/soldr-platform/src "
            "(expected package=soldr-platform binary=soldr_platform)"
        )

    # Integration owner declarations must identify a source-backed binary.
    # Library-unit owners use the Cargo underscore name and are validated by
    # the runtime nextest inventory instead.
    package_dirs = (
        {path.name for path in crates_root.iterdir()} if crates_root.is_dir() else set()
    )
    for owner in owners:
        if owner.binary == owner.package.replace("-", "_"):
            if owner.package not in package_dirs:
                failures.append(
                    f"stale target-run owner {owner.id}: missing package {owner.package}"
                )
        elif (owner.package, owner.binary) not in observed_binaries:
            failures.append(
                f"stale target-run owner {owner.id}: no integration binary "
                f"package={owner.package} binary={owner.binary}"
            )

    if failures:
        raise ValueError("\n".join(failures))


def append_summary(path: Path, selection: Selection) -> None:
    with path.open("a", encoding="utf-8", newline="\n") as stream:
        stream.write("\n### Native target-run ownership\n\n")
        stream.write(f"- Target: `{selection.target}`\n")
        stream.write(
            f"- Complete archive inventory: {selection.discovered_count} tests\n"
        )
        stream.write(
            f"- Positively owned for native replay: {selection.selected_count} tests\n"
        )
        stream.write("\n| Owner | Tests |\n| --- | ---: |\n")
        for owner_id, count in selection.owner_counts:
            stream.write(f"| `{owner_id}` | {count} |\n")


def write_filter(path: Path, expression: str) -> None:
    """Write one filter line without host-native newline translation."""

    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8", newline="\n") as stream:
        stream.write(expression)
        stream.write("\n")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--repo-root", type=Path, required=True)
    parser.add_argument("--list-json", type=Path)
    parser.add_argument("--target")
    parser.add_argument("--filter-output", type=Path)
    parser.add_argument("--github-summary", type=Path)
    parser.add_argument("--check-source-only", action="store_true")
    args = parser.parse_args()

    manifest = load_manifest(args.manifest)
    validate_source_ownership(manifest, args.repo_root)
    if args.check_source_only:
        return 0
    if args.list_json is None or args.target is None or args.filter_output is None:
        parser.error("selection requires --list-json, --target, and --filter-output")
    inventory = json.loads(args.list_json.read_text(encoding="utf-8"))
    if not isinstance(inventory, dict):
        raise ValueError("nextest list JSON must be an object")
    selection = build_selection(manifest, inventory, args.target)
    write_filter(args.filter_output, selection.filter_expression)
    if args.github_summary is not None:
        append_summary(args.github_summary, selection)
    print(
        "target-run ownership: "
        f"target={selection.target} discovered={selection.discovered_count} "
        f"selected={selection.selected_count}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
