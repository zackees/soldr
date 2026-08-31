#!/usr/bin/env python3
"""Build and verify Soldr's positive native target-run selection.

The nextest archive remains complete. Source classifications explain why every
integration module that touches real host facilities either runs once on the
canonical Linux host or needs native target replay. Only separate, positive
test/module replay selectors enter the target-run filter: classifying source
never implicitly selects a whole category binary.
"""

from __future__ import annotations

import argparse
import json
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Any

SCHEMA_VERSION = 2
SAFE_NAME = re.compile(r"^[A-Za-z0-9_-]+$")
SAFE_TEST = re.compile(r"^[A-Za-z0-9_:]+$")
SAFE_TARGET = re.compile(r"^[A-Za-z0-9_.-]+$")
MODULE_DECLARATION = re.compile(r"(?m)^\s*(?:pub\s+)?mod\s+([A-Za-z0-9_]+)\s*;")
HOST_CFG = re.compile(
    r"cfg(?:_attr)?\s*\([^\n]*(?:windows|unix|target_os|target_arch|target_env)"
)
DISPOSITIONS = {"native-linux-once", "target-replay"}

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
class SourceClassification:
    id: str
    package: str
    binary: str
    disposition: str
    reason: str
    modules: tuple[str, ...] | None = None

    def source_keys(self) -> tuple[tuple[str, str, str | None], ...]:
        if self.modules is None:
            return ((self.package, self.binary, None),)
        return tuple((self.package, self.binary, module) for module in self.modules)

    def contains_test(self, test_name: str) -> bool:
        return self.modules is None or any(
            test_name.startswith(f"{module}::") for module in self.modules
        )


@dataclass(frozen=True)
class ReplaySelector:
    id: str
    source: SourceClassification
    reason: str
    test_name: str | None = None
    test_prefix: str | None = None
    targets: tuple[str, ...] | None = None

    def applies_to(self, target: str) -> bool:
        return self.targets is None or target in self.targets

    def matches(self, package: str, binary: str, test_name: str) -> bool:
        if self.source.package != package or self.source.binary != binary:
            return False
        if self.test_name is not None:
            return test_name == self.test_name
        assert self.test_prefix is not None
        return test_name.startswith(self.test_prefix)

    def filter_expression(self) -> str:
        expression = f"package({self.source.package}) & binary({self.source.binary})"
        if self.test_name is not None:
            expression += f" & test(/^{self.test_name}$/)"
        else:
            expression += f" & test(/^{self.test_prefix}/)"
        return f"({expression})"


@dataclass(frozen=True)
class OwnershipManifest:
    classifications: tuple[SourceClassification, ...]
    selectors: tuple[ReplaySelector, ...]


@dataclass(frozen=True)
class Selection:
    target: str
    discovered_count: int
    selected_count: int
    test_ids: tuple[str, ...]
    filter_expression: str
    selector_counts: tuple[tuple[str, int], ...]


def _require_nonempty_string(value: Any, label: str) -> str:
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f"{label} must be a non-empty string")
    return value


def _parse_targets(value: Any, label: str) -> tuple[str, ...] | None:
    if value is None:
        return None
    if not isinstance(value, list) or not value:
        raise ValueError(f"{label} must be a non-empty array")
    targets = tuple(
        _require_nonempty_string(target, f"{label}[{index}]")
        for index, target in enumerate(value)
    )
    if any(not SAFE_TARGET.fullmatch(target) for target in targets):
        raise ValueError(f"{label} contains an unsafe target triple")
    if len(set(targets)) != len(targets):
        raise ValueError(f"{label} contains duplicates")
    return targets


def _target_scopes_overlap(
    left: tuple[str, ...] | None, right: tuple[str, ...] | None
) -> bool:
    return left is None or right is None or not set(left).isdisjoint(right)


def _selector_patterns_overlap(left: ReplaySelector, right: ReplaySelector) -> bool:
    if (
        left.source.package != right.source.package
        or left.source.binary != right.source.binary
    ):
        return False
    if not _target_scopes_overlap(left.targets, right.targets):
        return False
    if left.test_name is not None and right.test_name is not None:
        return left.test_name == right.test_name
    if left.test_prefix is not None and right.test_prefix is not None:
        return left.test_prefix.startswith(
            right.test_prefix
        ) or right.test_prefix.startswith(left.test_prefix)
    exact = left.test_name if left.test_name is not None else right.test_name
    prefix = left.test_prefix if left.test_prefix is not None else right.test_prefix
    assert exact is not None and prefix is not None
    return exact.startswith(prefix)


def parse_manifest(payload: dict[str, object]) -> OwnershipManifest:
    if payload.get("schema_version") != SCHEMA_VERSION:
        raise ValueError(
            f"target-run ownership schema_version must be {SCHEMA_VERSION}"
        )
    if payload.get("policy_issue") != "soldr#2999":
        raise ValueError("target-run ownership policy_issue must be soldr#2999")
    raw_classifications = payload.get("source_classifications")
    raw_selectors = payload.get("replay_selectors")
    if not isinstance(raw_classifications, list):
        raise ValueError("target-run source_classifications must be an array")
    if not isinstance(raw_selectors, list):
        raise ValueError("target-run replay_selectors must be an array")

    classifications: list[SourceClassification] = []
    classifications_by_id: dict[str, SourceClassification] = {}
    classified_sources: dict[tuple[str, str, str | None], str] = {}
    for index, raw in enumerate(raw_classifications):
        label = f"source_classifications[{index}]"
        if not isinstance(raw, dict):
            raise ValueError(f"{label} must be an object")
        allowed = {"id", "package", "binary", "modules", "disposition", "reason"}
        extra = set(raw) - allowed
        missing = {"id", "package", "binary", "disposition", "reason"} - set(raw)
        if extra or missing:
            raise ValueError(
                f"{label} keys disagree: missing={sorted(missing)} extra={sorted(extra)}"
            )
        classification_id = _require_nonempty_string(raw["id"], f"{label}.id")
        package = _require_nonempty_string(raw["package"], f"{label}.package")
        binary = _require_nonempty_string(raw["binary"], f"{label}.binary")
        disposition = _require_nonempty_string(
            raw["disposition"], f"{label}.disposition"
        )
        reason = _require_nonempty_string(raw["reason"], f"{label}.reason")
        if not SAFE_NAME.fullmatch(classification_id):
            raise ValueError(f"{label}.id contains unsafe characters")
        if not SAFE_NAME.fullmatch(package) or not SAFE_NAME.fullmatch(binary):
            raise ValueError(
                f"{label} package/binary contains unsafe filter characters"
            )
        if disposition not in DISPOSITIONS:
            raise ValueError(
                f"{label}.disposition must be one of {sorted(DISPOSITIONS)}"
            )
        raw_modules = raw.get("modules")
        modules: tuple[str, ...] | None
        if raw_modules is None:
            modules = None
        elif not isinstance(raw_modules, list) or not raw_modules:
            raise ValueError(f"{label}.modules must be a non-empty array")
        else:
            modules = tuple(
                _require_nonempty_string(module, f"{label}.modules[{module_index}]")
                for module_index, module in enumerate(raw_modules)
            )
            if any(not SAFE_NAME.fullmatch(module) for module in modules):
                raise ValueError(f"{label}.modules contains unsafe characters")
            if len(set(modules)) != len(modules):
                raise ValueError(f"{label}.modules contains duplicates")
        if classification_id in classifications_by_id:
            raise ValueError(f"duplicate source classification id: {classification_id}")
        classification = SourceClassification(
            classification_id, package, binary, disposition, reason, modules
        )
        for source_key in classification.source_keys():
            if source_key in classified_sources:
                raise ValueError(
                    "overlapping source classifications: "
                    f"{classified_sources[source_key]} and {classification_id} both classify "
                    f"{source_key}"
                )
            classified_sources[source_key] = classification_id
        classifications.append(classification)
        classifications_by_id[classification_id] = classification

    selectors: list[ReplaySelector] = []
    selector_ids: set[str] = set()
    for index, raw in enumerate(raw_selectors):
        label = f"replay_selectors[{index}]"
        if not isinstance(raw, dict):
            raise ValueError(f"{label} must be an object")
        allowed = {
            "id",
            "source_id",
            "test_name",
            "test_prefix",
            "targets",
            "reason",
        }
        extra = set(raw) - allowed
        missing = {"id", "source_id", "reason"} - set(raw)
        if extra or missing:
            raise ValueError(
                f"{label} keys disagree: missing={sorted(missing)} extra={sorted(extra)}"
            )
        selector_id = _require_nonempty_string(raw["id"], f"{label}.id")
        source_id = _require_nonempty_string(raw["source_id"], f"{label}.source_id")
        reason = _require_nonempty_string(raw["reason"], f"{label}.reason")
        if not SAFE_NAME.fullmatch(selector_id):
            raise ValueError(f"{label}.id contains unsafe characters")
        if selector_id in selector_ids:
            raise ValueError(f"duplicate replay selector id: {selector_id}")
        selector_ids.add(selector_id)
        source = classifications_by_id.get(source_id)
        if source is None:
            raise ValueError(
                f"{label}.source_id refers to stale classification {source_id}"
            )
        if source.disposition != "target-replay":
            raise ValueError(
                f"{label} selects {source_id}, which is classified native-linux-once"
            )
        has_name = "test_name" in raw
        has_prefix = "test_prefix" in raw
        if has_name == has_prefix:
            raise ValueError(
                f"{label} must contain exactly one of test_name/test_prefix"
            )
        test_name = (
            _require_nonempty_string(raw["test_name"], f"{label}.test_name")
            if has_name
            else None
        )
        test_prefix = (
            _require_nonempty_string(raw["test_prefix"], f"{label}.test_prefix")
            if has_prefix
            else None
        )
        pattern = test_name if test_name is not None else test_prefix
        assert pattern is not None
        if not SAFE_TEST.fullmatch(pattern):
            raise ValueError(f"{label} selector contains unsafe filter characters")
        if not source.contains_test(pattern):
            raise ValueError(
                f"{label} selector {pattern!r} is outside classified source {source_id}"
            )
        selector = ReplaySelector(
            selector_id,
            source,
            reason,
            test_name,
            test_prefix,
            _parse_targets(raw.get("targets"), f"{label}.targets"),
        )
        for earlier in selectors:
            if _selector_patterns_overlap(earlier, selector):
                raise ValueError(
                    f"overlapping replay selectors: {earlier.id} and {selector.id}"
                )
        selectors.append(selector)

    for classification in classifications:
        if classification.disposition != "target-replay":
            continue
        source_selectors = [
            selector
            for selector in selectors
            if selector.source.id == classification.id
        ]
        if not source_selectors:
            raise ValueError(
                f"target-replay classification {classification.id} has no positive selector"
            )
        for module in classification.modules or ():
            module_prefix = f"{module}::"
            if not any(
                (selector.test_name or selector.test_prefix or "").startswith(
                    module_prefix
                )
                for selector in source_selectors
            ):
                raise ValueError(
                    f"target-replay source {classification.id} module {module} "
                    "has no positive selector"
                )
    return OwnershipManifest(tuple(classifications), tuple(selectors))


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
    parsed = parse_manifest(manifest)
    selectors = tuple(
        selector for selector in parsed.selectors if selector.applies_to(target)
    )
    tests = _inventory_tests(inventory)
    selected: set[tuple[str, str, str]] = set()
    selector_counts: list[tuple[str, int]] = []
    for selector in selectors:
        matches = {test for test in tests if selector.matches(*test)}
        if not matches:
            raise ValueError(
                f"target-run selector {selector.id!r} matched no tests for target {target}; "
                "the selector is stale or the archive lost required native coverage"
            )
        overlap = selected.intersection(matches)
        if overlap:
            example = "::".join(sorted(overlap)[0])
            raise ValueError(
                f"target-run selector {selector.id!r} overlaps an earlier selector at {example}"
            )
        selected.update(matches)
        selector_counts.append((selector.id, len(matches)))
    if not selected:
        raise ValueError(f"target-run ownership selects zero tests for target {target}")

    test_ids = tuple("::".join(test) for test in sorted(selected))
    expression = " | ".join(selector.filter_expression() for selector in selectors)
    return Selection(
        target=target,
        discovered_count=len(tests),
        selected_count=len(selected),
        test_ids=test_ids,
        filter_expression=expression,
        selector_counts=tuple(selector_counts),
    )


def _is_host_sensitive(source: str) -> bool:
    return HOST_CFG.search(source) is not None or any(
        marker in source for marker in HOST_MARKERS
    )


def validate_source_ownership(manifest: dict[str, object], repo_root: Path) -> None:
    """Fail on unclassified host source or a stale/overlapping classification."""

    parsed = parse_manifest(manifest)
    classifications = {
        key: classification
        for classification in parsed.classifications
        for key in classification.source_keys()
    }
    crates_root = repo_root / "crates"
    failures: list[str] = []
    observed_sources: set[tuple[str, str, str | None]] = set()
    host_sensitive_sources: set[tuple[str, str, str | None]] = set()
    category_binaries: set[tuple[str, str]] = set()

    if crates_root.is_dir():
        for crate in sorted(path for path in crates_root.iterdir() if path.is_dir()):
            package = crate.name
            tests_root = crate / "tests"
            if not tests_root.is_dir():
                continue

            for main_path in sorted(tests_root.glob("*/main.rs")):
                binary = main_path.parent.name
                category_binaries.add((package, binary))
                main_source = main_path.read_text(encoding="utf-8")
                for module in MODULE_DECLARATION.findall(main_source):
                    if module == "common":
                        continue
                    module_path = main_path.parent / f"{module}.rs"
                    source_key = (package, binary, module)
                    observed_sources.add(source_key)
                    if not module_path.is_file():
                        failures.append(
                            f"{main_path.relative_to(repo_root)} declares missing {module}.rs"
                        )
                        continue
                    if _is_host_sensitive(module_path.read_text(encoding="utf-8")):
                        host_sensitive_sources.add(source_key)

            for source_path in sorted(tests_root.glob("*.rs")):
                binary = source_path.stem
                source_key = (package, binary, None)
                observed_sources.add(source_key)
                if _is_host_sensitive(source_path.read_text(encoding="utf-8")):
                    host_sensitive_sources.add(source_key)

    platform_root = repo_root / "crates" / "soldr-platform"
    if platform_root.is_dir():
        platform_key = ("soldr-platform", "soldr_platform", None)
        observed_sources.add(platform_key)
        host_sensitive_sources.add(platform_key)

    for source_key in sorted(host_sensitive_sources):
        if source_key not in classifications:
            package, binary, module = source_key
            module_note = f" module={module}" if module is not None else ""
            failures.append(
                "unclassified host-sensitive test source: "
                f"package={package} binary={binary}{module_note}"
            )

    for source_key, classification in classifications.items():
        if source_key not in observed_sources:
            failures.append(
                f"stale source classification {classification.id}: no source for {source_key}"
            )
        if (
            source_key[2] is None
            and (source_key[0], source_key[1]) in category_binaries
        ):
            failures.append(
                f"source classification {classification.id} covers category binary "
                f"{source_key[0]}::{source_key[1]} without explicit modules"
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
            f"- Positively selected for native replay: {selection.selected_count} tests\n"
        )
        stream.write("\n| Replay selector | Tests |\n| --- | ---: |\n")
        for selector_id, count in selection.selector_counts:
            stream.write(f"| `{selector_id}` | {count} |\n")


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
