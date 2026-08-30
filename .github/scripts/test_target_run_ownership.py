#!/usr/bin/env python3
"""Tests for the positive native target-run ownership contract."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

import target_run_ownership as ownership


def inventory(*suites: dict[str, object]) -> dict[str, object]:
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


def manifest(*owners: dict[str, object]) -> dict[str, object]:
    return {
        "schema_version": 1,
        "policy_issue": "soldr#2999",
        "owners": list(owners),
    }


class TargetRunOwnershipTests(unittest.TestCase):
    def test_positive_owners_select_only_declared_tests(self) -> None:
        declared = manifest(
            {
                "id": "broker-contracts",
                "package": "soldr-cli",
                "binary": "broker",
                "reason": "real process and IPC behavior",
            },
            {
                "id": "one-platform-contract",
                "package": "soldr-platform",
                "binary": "soldr_platform",
                "test_prefix": "platform_win::process::",
                "reason": "Windows process primitives",
            },
        )
        discovered = inventory(
            suite("soldr-cli", "broker", "cli_broker::starts", "cli_broker::stops"),
            suite("soldr-cli", "guards", "version_lockstep::matches"),
            suite(
                "soldr-platform",
                "soldr_platform",
                "platform_win::process::kills_tree",
                "portable::parses_name",
            ),
        )

        selected = ownership.build_selection(
            declared, discovered, "x86_64-pc-windows-msvc"
        )

        self.assertEqual(selected.discovered_count, 5)
        self.assertEqual(selected.selected_count, 3)
        self.assertEqual(
            selected.test_ids,
            (
                "soldr-cli::broker::cli_broker::starts",
                "soldr-cli::broker::cli_broker::stops",
                "soldr-platform::soldr_platform::platform_win::process::kills_tree",
            ),
        )
        self.assertIn("package(soldr-cli) & binary(broker)", selected.filter_expression)
        self.assertIn("test(/^platform_win::process::/)", selected.filter_expression)

    def test_stale_owner_is_fatal_instead_of_silently_dropping_coverage(self) -> None:
        declared = manifest(
            {
                "id": "renamed-module",
                "package": "soldr-cli",
                "binary": "broker",
                "test_prefix": "old_name::",
                "reason": "must not decay silently",
            }
        )
        discovered = inventory(suite("soldr-cli", "broker", "new_name::works"))

        with self.assertRaisesRegex(ValueError, "matched no tests"):
            ownership.build_selection(declared, discovered, "aarch64-apple-darwin")

    def test_platform_specific_owner_applies_only_to_declared_targets(self) -> None:
        declared = manifest(
            {
                "id": "windows-pipe",
                "package": "soldr-daemon",
                "binary": "daemon_windows_pipe_peer",
                "targets": ["x86_64-pc-windows-msvc"],
                "reason": "Windows named-pipe peer identity",
            },
            {
                "id": "portable-native-contract",
                "package": "soldr-platform",
                "binary": "soldr_platform",
                "reason": "host primitives on every target",
            },
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

    def test_empty_ownership_is_fatal(self) -> None:
        with self.assertRaisesRegex(ValueError, "selects zero tests"):
            ownership.build_selection(
                manifest(), inventory(), "aarch64-unknown-linux-gnu"
            )

    def test_inverse_guard_rejects_unowned_host_sensitive_module(self) -> None:
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
                ValueError, "unowned host-sensitive test source"
            ):
                ownership.validate_source_ownership(manifest(), root)

            declared = manifest(
                {
                    "id": "demo-native",
                    "package": "demo",
                    "binary": "native",
                    "reason": "real child lifecycle",
                }
            )
            ownership.validate_source_ownership(declared, root)

    def test_repository_manifest_covers_host_sensitive_sources(self) -> None:
        repo_root = Path(__file__).resolve().parents[2]
        declared = ownership.load_manifest(
            repo_root / "ci" / "target-run-ownership.json"
        )

        ownership.validate_source_ownership(declared, repo_root)


if __name__ == "__main__":
    unittest.main()
