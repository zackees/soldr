import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = ROOT / ".github" / "workflows" / "coverage.yml"
COVERAGE_RUNNER = ROOT / "ci" / "test.py"


def workflow_step(workflow: str, name: str) -> str:
    marker = f"      - name: {name}\n"
    _, remainder = workflow.split(marker, maxsplit=1)
    return marker + remainder.split("\n      - ", maxsplit=1)[0]


class TestCoverageWorkflowContract(unittest.TestCase):
    def test_profraw_defenses_remain_without_core_dump_triage(self) -> None:
        """#764: retain cheap profraw evidence after soaked core triage is removed."""
        workflow = WORKFLOW.read_text(encoding="utf-8")
        coverage_runner = COVERAGE_RUNNER.read_text(encoding="utf-8")

        for expensive_instrumentation in (
            "Enable core dumps",
            "kernel.core_pattern",
            "ulimit -c",
            "logs/core.",
            "apt-get install -y -q gdb",
            "gdb -batch",
        ):
            self.assertNotIn(expensive_instrumentation, workflow)

        coverage_run = workflow_step(workflow, "Run tests with coverage")
        self.assertIn("ci.test --coverage", coverage_run)

        profraw_diagnostics = workflow_step(workflow, "Coverage profraw diagnostics")
        for retained_diagnostic in (
            "if: ${{ failure() }}",
            "llvm-profdata identity",
            '"$PROFDATA" --version',
            "per-profraw validation bisect",
            '"$PROFDATA" show "$f"',
            "logs/bad-profraw/manifest.json",
            "merge retry excluding bad files",
        ):
            self.assertIn(retained_diagnostic, profraw_diagnostics)

        rejected_upload = workflow_step(workflow, "Upload rejected profraw evidence")
        for retained_upload_setting in (
            "if: ${{ always() }}",
            "uses: actions/upload-artifact@v4",
            "name: coverage-bad-profraw",
            "path: logs/bad-profraw/**",
        ):
            self.assertIn(retained_upload_setting, rejected_upload)

        for retained_runner_defense in (
            "def _prune_invalid_profraw(",
            '"llvm_version_command"',
            '"github_run_id"',
            '"github_commit"',
            '"rejected_profiles"',
            "profraw.unlink()",
            '_prune_invalid_profraw(ROOT / "target" / "llvm-cov-target")',
        ):
            self.assertIn(retained_runner_defense, coverage_runner)


if __name__ == "__main__":
    unittest.main()
