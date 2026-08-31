#!/usr/bin/env python3
"""Tests for the positive native target-run ownership contract."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from typing import Any

import target_run_ownership as ownership


def inventory(*suites: dict[str, Any]) -> dict[str, object]:
    return {
        "test-count": sum(len(suite["testcases"]) for suite in suites),
        "rust-suites": {f"suite-{index}": suite for index, suite in enumerate(suites)},
    }


def suite(package: str, binary: str, *tests: str) -> dict[str, object]:
    return {
        "package-name": package,
        "binary-name": binary,
        "testcases": {name: {"ignored": False} for name in tests},
    }


def classification(
    classification_id: str,
    package: str,
    binary: str,
    disposition: str,
    *,
    modules: list[str] | None = None,
) -> dict[str, object]:
    result: dict[str, object] = {
        "id": classification_id,
        "package": package,
        "binary": binary,
        "disposition": disposition,
        "reason": f"Focused reason for {classification_id}.",
    }
    if modules is not None:
        result["modules"] = modules
    return result


def selector(
    selector_id: str,
    source_id: str,
    *,
    test_name: str | None = None,
    test_prefix: str | None = None,
    targets: list[str] | None = None,
) -> dict[str, object]:
    result: dict[str, object] = {
        "id": selector_id,
        "source_id": source_id,
        "reason": f"Focused reason for {selector_id}.",
    }
    if test_name is not None:
        result["test_name"] = test_name
    if test_prefix is not None:
        result["test_prefix"] = test_prefix
    if targets is not None:
        result["targets"] = targets
    return result


def manifest(
    *classifications: dict[str, object],
    selectors: list[dict[str, object]] | None = None,
) -> dict[str, object]:
    return {
        "schema_version": 2,
        "policy_issue": "soldr#2999",
        "source_classifications": list(classifications),
        "replay_selectors": selectors or [],
    }


class TargetRunOwnershipTests(unittest.TestCase):
    def test_source_classification_does_not_implicitly_select_a_whole_binary(
        self,
    ) -> None:
        declared = manifest(
            classification(
                "broker-native-once",
                "soldr-cli",
                "broker",
                "native-linux-once",
                modules=["portable_process_contract"],
            ),
            classification(
                "broker-target-replay",
                "soldr-cli",
                "broker",
                "target-replay",
                modules=["kill_contract"],
            ),
            selectors=[
                selector(
                    "broker-kill-smoke",
                    "broker-target-replay",
                    test_name="kill_contract::recovers_after_kill",
                )
            ],
        )
        discovered = inventory(
            suite(
                "soldr-cli",
                "broker",
                "portable_process_contract::parses_status",
                "kill_contract::recovers_after_kill",
                "kill_contract::portable_planning_helper",
            )
        )

        selected = ownership.build_selection(
            declared, discovered, "x86_64-apple-darwin"
        )

        self.assertEqual(
            selected.test_ids,
            ("soldr-cli::broker::kill_contract::recovers_after_kill",),
        )
        self.assertIn(
            "test(/^kill_contract::recovers_after_kill$/)",
            selected.filter_expression,
        )

    def test_positive_module_prefix_selects_only_the_declared_module(self) -> None:
        declared = manifest(
            classification(
                "platform-native",
                "soldr-platform",
                "soldr_platform",
                "target-replay",
            ),
            selectors=[
                selector(
                    "windows-process-module",
                    "platform-native",
                    test_prefix="platform_win::process::",
                )
            ],
        )
        discovered = inventory(
            suite(
                "soldr-platform",
                "soldr_platform",
                "platform_win::process::kills_tree",
                "platform_win::process::reads_pid",
                "portable::parses_name",
            )
        )

        selected = ownership.build_selection(
            declared, discovered, "x86_64-pc-windows-msvc"
        )

        self.assertEqual(selected.selected_count, 2)
        self.assertTrue(
            all("platform_win::process::" in test for test in selected.test_ids)
        )

    def test_stale_selector_is_fatal_instead_of_silently_dropping_coverage(
        self,
    ) -> None:
        declared = manifest(
            classification(
                "broker-native",
                "soldr-cli",
                "broker",
                "target-replay",
                modules=["new_name"],
            ),
            selectors=[
                selector(
                    "renamed-test",
                    "broker-native",
                    test_name="new_name::old_test",
                )
            ],
        )
        discovered = inventory(suite("soldr-cli", "broker", "new_name::works"))

        with self.assertRaisesRegex(ValueError, "matched no tests"):
            ownership.build_selection(declared, discovered, "aarch64-apple-darwin")

    def test_platform_specific_selector_applies_only_to_declared_targets(self) -> None:
        declared = manifest(
            classification(
                "pipe-native",
                "soldr-daemon",
                "daemon_windows_pipe_peer",
                "target-replay",
            ),
            classification(
                "platform-native", "soldr-platform", "soldr_platform", "target-replay"
            ),
            selectors=[
                selector(
                    "windows-pipe",
                    "pipe-native",
                    test_prefix="accepted_pipe_",
                    targets=["x86_64-pc-windows-msvc"],
                ),
                selector(
                    "platform-host",
                    "platform-native",
                    test_prefix="host::",
                ),
            ],
        )
        discovered = inventory(
            suite(
                "soldr-daemon",
                "daemon_windows_pipe_peer",
                "accepted_pipe_reports_the_os_observed_client",
            ),
            suite("soldr-platform", "soldr_platform", "host::facts::matches"),
        )

        windows = ownership.build_selection(
            declared, discovered, "x86_64-pc-windows-msvc"
        )
        darwin = ownership.build_selection(declared, discovered, "x86_64-apple-darwin")

        self.assertEqual(windows.selected_count, 2)
        self.assertEqual(darwin.selected_count, 1)
        self.assertNotIn("daemon_windows_pipe_peer", darwin.filter_expression)

    def test_filter_file_is_one_lf_terminated_line(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "filter.txt"
            ownership.write_filter(path, "package(soldr-cli) & binary(broker)")

            self.assertEqual(
                path.read_bytes(), b"package(soldr-cli) & binary(broker)\n"
            )

    def test_empty_target_selection_is_fatal(self) -> None:
        declared = manifest(
            classification(
                "windows-native", "soldr-platform", "soldr_platform", "target-replay"
            ),
            selectors=[
                selector(
                    "windows-only",
                    "windows-native",
                    test_prefix="platform_win::",
                    targets=["x86_64-pc-windows-msvc"],
                )
            ],
        )
        discovered = inventory(
            suite(
                "soldr-platform",
                "soldr_platform",
                "platform_win::process::works",
            )
        )

        with self.assertRaisesRegex(ValueError, "selects zero tests"):
            ownership.build_selection(declared, discovered, "aarch64-unknown-linux-gnu")

    def test_inverse_guard_requires_explicit_classification_but_not_replay(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            category = root / "crates" / "demo" / "tests" / "native"
            category.mkdir(parents=True)
            (category / "main.rs").write_text("mod os_contract;\n", encoding="utf-8")
            (category / "os_contract.rs").write_text(
                "#[test]\nfn child_lifetime() {\n"
                '    let _ = std::process::Command::new("demo");\n'
                "}\n",
                encoding="utf-8",
            )

            with self.assertRaisesRegex(
                ValueError, "unclassified host-sensitive test source"
            ):
                ownership.validate_source_ownership(manifest(), root)

            declared = manifest(
                classification(
                    "demo-linux-once",
                    "demo",
                    "native",
                    "native-linux-once",
                    modules=["os_contract"],
                )
            )
            ownership.validate_source_ownership(declared, root)

    def test_target_replay_classification_requires_a_positive_selector(self) -> None:
        declared = manifest(
            classification(
                "orphan-replay",
                "soldr-cli",
                "broker",
                "target-replay",
                modules=["cli_kill_matrix"],
            )
        )

        with self.assertRaisesRegex(ValueError, "has no positive selector"):
            ownership.parse_manifest(declared)

    def test_every_module_in_a_target_replay_class_requires_a_selector(self) -> None:
        declared = manifest(
            classification(
                "lifecycle-native",
                "soldr-cli",
                "broker",
                "target-replay",
                modules=["cli_kill_matrix", "cli_broker_stop"],
            ),
            selectors=[
                selector(
                    "kill-only",
                    "lifecycle-native",
                    test_name="cli_kill_matrix::recovers",
                )
            ],
        )

        with self.assertRaisesRegex(
            ValueError, "module cli_broker_stop has no positive selector"
        ):
            ownership.parse_manifest(declared)

    def test_native_linux_once_source_cannot_be_selected_for_replay(self) -> None:
        declared = manifest(
            classification(
                "portable-process",
                "soldr-cli",
                "broker",
                "native-linux-once",
                modules=["cli_broker_status"],
            ),
            selectors=[
                selector(
                    "bad-selector",
                    "portable-process",
                    test_prefix="cli_broker_status::",
                )
            ],
        )

        with self.assertRaisesRegex(ValueError, "classified native-linux-once"):
            ownership.parse_manifest(declared)

    def test_selector_must_stay_inside_its_classified_modules(self) -> None:
        declared = manifest(
            classification(
                "kill-native",
                "soldr-cli",
                "broker",
                "target-replay",
                modules=["cli_kill_matrix"],
            ),
            selectors=[
                selector(
                    "outside-source",
                    "kill-native",
                    test_prefix="cli_broker_status::",
                )
            ],
        )

        with self.assertRaisesRegex(ValueError, "outside classified source"):
            ownership.parse_manifest(declared)

    def test_overlapping_source_classifications_are_rejected(self) -> None:
        declared = manifest(
            classification(
                "first", "soldr-cli", "broker", "native-linux-once", modules=["same"]
            ),
            classification(
                "second", "soldr-cli", "broker", "native-linux-once", modules=["same"]
            ),
        )

        with self.assertRaisesRegex(ValueError, "overlapping source classifications"):
            ownership.parse_manifest(declared)

    def test_overlapping_replay_selectors_are_rejected_before_inventory(self) -> None:
        declared = manifest(
            classification(
                "kill-native",
                "soldr-cli",
                "broker",
                "target-replay",
                modules=["cli_kill_matrix"],
            ),
            selectors=[
                selector(
                    "whole-module",
                    "kill-native",
                    test_prefix="cli_kill_matrix::",
                ),
                selector(
                    "one-test",
                    "kill-native",
                    test_name="cli_kill_matrix::recovers",
                ),
            ],
        )

        with self.assertRaisesRegex(ValueError, "overlapping replay selectors"):
            ownership.parse_manifest(declared)

    def test_unsafe_filter_characters_are_rejected(self) -> None:
        declared = manifest(
            classification(
                "kill-native",
                "soldr-cli",
                "broker",
                "target-replay",
                modules=["cli_kill_matrix"],
            ),
            selectors=[
                selector(
                    "unsafe",
                    "kill-native",
                    test_prefix="cli_kill_matrix::.*",
                )
            ],
        )

        with self.assertRaisesRegex(ValueError, "unsafe filter characters"):
            ownership.parse_manifest(declared)

    def test_stale_source_classification_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "crates" / "demo" / "tests").mkdir(parents=True)
            declared = manifest(
                classification("missing-source", "demo", "gone", "native-linux-once")
            )

            with self.assertRaisesRegex(ValueError, "stale source classification"):
                ownership.validate_source_ownership(declared, root)

    def test_category_classification_must_name_modules(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            category = root / "crates" / "demo" / "tests" / "native"
            category.mkdir(parents=True)
            (category / "main.rs").write_text("mod os_contract;\n", encoding="utf-8")
            (category / "os_contract.rs").write_text(
                "#[test]\nfn works() {}\n", encoding="utf-8"
            )
            declared = manifest(
                classification("whole-category", "demo", "native", "native-linux-once")
            )

            with self.assertRaisesRegex(ValueError, "without explicit modules"):
                ownership.validate_source_ownership(declared, root)

    def test_repository_manifest_covers_host_sensitive_sources(self) -> None:
        repo_root = Path(__file__).resolve().parents[2]
        declared = ownership.load_manifest(
            repo_root / "ci" / "target-run-ownership.json"
        )

        ownership.validate_source_ownership(declared, repo_root)


if __name__ == "__main__":
    unittest.main()
