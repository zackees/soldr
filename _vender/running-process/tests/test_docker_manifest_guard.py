"""Regression tests for the Linux Docker manifest-cache coverage guard (#772)."""

from __future__ import annotations

import unittest

from ci.docker_manifest_guard import (
    DOCKERFILE,
    MANIFEST_STAGE,
    WORKSPACE_MANIFEST,
    copied_paths,
    is_covered,
    missing_inputs,
    stage_body,
    workspace_members,
)


class TestWorkspaceMembers(unittest.TestCase):
    def test_reads_every_member_from_the_root_manifest(self) -> None:
        members = workspace_members(WORKSPACE_MANIFEST.read_text(encoding="utf-8"))
        self.assertIn("crates/running-process", members)
        self.assertIn("crates/running-process-probe-daemon", members)
        self.assertIn("testbins", members)

    def test_stops_at_the_next_table(self) -> None:
        members = workspace_members(
            '[workspace]\nresolver = "2"\nmembers = ["a", "b"]\n'
            '\n[workspace.dependencies]\nmembers = ["not-a-member"]\n'
        )
        self.assertEqual(members, ["a", "b"])


class TestStageParsing(unittest.TestCase):
    def test_reads_only_the_named_stage(self) -> None:
        body = stage_body(
            "FROM base AS one\nCOPY one.toml /work/\n\nFROM one AS two\nCOPY two.toml /work/\n",
            "one",
        )
        self.assertIn("one.toml", body)
        self.assertNotIn("two.toml", body)

    def test_joins_line_continuations_and_drops_the_destination(self) -> None:
        paths = copied_paths("COPY a/Cargo.toml \\\n    b/Cargo.toml \\\n    /work/\n")
        self.assertEqual(paths, {"a/Cargo.toml", "b/Cargo.toml"})

    def test_drops_flags(self) -> None:
        copied = copied_paths("COPY --from=deps /work/target /work/target")
        self.assertEqual(copied, {"/work/target"})

    def test_directory_copy_covers_paths_beneath_it(self) -> None:
        self.assertTrue(is_covered("crates/x/proto", {"crates/x/proto"}))
        self.assertTrue(is_covered("crates/x/proto/v1.proto", {"crates/x/proto"}))
        self.assertFalse(is_covered("crates/x/Cargo.toml", {"crates/x/proto"}))


class TestCoverage(unittest.TestCase):
    def test_current_dockerfile_covers_every_workspace_member(self) -> None:
        members = workspace_members(WORKSPACE_MANIFEST.read_text(encoding="utf-8"))
        copied = copied_paths(stage_body(DOCKERFILE.read_text(encoding="utf-8"), MANIFEST_STAGE))
        self.assertEqual(missing_inputs(members, copied), [])

    def test_an_omitted_member_is_reported(self) -> None:
        """The guard must fail when a member is added but the Dockerfile is not."""
        copied = copied_paths(stage_body(DOCKERFILE.read_text(encoding="utf-8"), MANIFEST_STAGE))
        missing = missing_inputs(["crates/some-new-crate"], copied)
        self.assertEqual(missing, ["crates/some-new-crate/Cargo.toml"])

    def test_build_script_and_proto_inputs_are_required(self) -> None:
        """A member with build.rs + proto/ needs those copied too, not just the manifest."""
        member = "crates/running-process-probe"
        missing = missing_inputs([member], {f"{member}/Cargo.toml"})
        self.assertEqual(missing, [f"{member}/build.rs", f"{member}/proto"])


if __name__ == "__main__":
    unittest.main()
