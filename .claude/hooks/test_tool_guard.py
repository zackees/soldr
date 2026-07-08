#!/usr/bin/env -S uv run --no-project --script
"""Unit tests for the shell command tool guard hook.

Run via uv (sibling-module resolution works because cwd is the hook dir):

    uv run --no-project --directory .claude/hooks python -m unittest test_tool_guard
"""

import unittest
from pathlib import Path

from tool_guard import (  # type: ignore[import-not-found]
    SHELL_TOOL_NAMES,
    check_command,
    extract_command,
)

REPO_ROOT = Path(__file__).resolve().parents[2]
DEV_LOOP_SCRIPTS = ("lint", "test", "install")
UV_RUN_GUARDS = ("--no-project", "--no-sync", "--frozen")


class ToolGuardTests(unittest.TestCase):
    def test_blocks_bare_rust_tool(self):
        result = check_command("cargo test")
        self.assertIsNotNone(result)
        self.assertEqual(result[0], "cargo")

    def test_blocks_bare_rustc(self):
        result = check_command("rustc --version")
        self.assertIsNotNone(result)
        self.assertEqual(result[0], "rustc")

    def test_blocks_bare_rustfmt(self):
        result = check_command("rustfmt --check src/lib.rs")
        self.assertIsNotNone(result)
        self.assertEqual(result[0], "rustfmt")

    def test_blocks_bare_clippy_driver(self):
        result = check_command("clippy-driver --version")
        self.assertIsNotNone(result)
        self.assertEqual(result[0], "clippy-driver")

    def test_blocks_bare_cargo_clippy(self):
        result = check_command("cargo-clippy --version")
        self.assertIsNotNone(result)
        self.assertEqual(result[0], "cargo-clippy")

    def test_blocks_bare_cargo_fmt(self):
        result = check_command("cargo-fmt --check")
        self.assertIsNotNone(result)
        self.assertEqual(result[0], "cargo-fmt")

    def test_blocks_uv_run_rust_tool_shim(self):
        commands = (
            "uv run cargo test",
            "uv run -- cargo test",
            "uv run --offline cargo build",
            "uv run --with foo cargo test",
            "uv run --with=foo cargo test",
            "uv run --isolated cargo build",
            "uv run --project . cargo check",
            "uv run -q cargo test",
            "uv run -- --offline cargo test",
        )
        for command in commands:
            with self.subTest(command=command):
                result = check_command(command)
                self.assertIsNotNone(result)
                self.assertEqual(result[0], "cargo")

    def test_allows_soldr_wrapped_rust_tool(self):
        self.assertIsNone(check_command("soldr cargo test"))
        self.assertIsNone(check_command("soldr --no-cache cargo build"))
        self.assertIsNone(check_command("soldr rustc --version"))
        self.assertIsNone(check_command("soldr rustfmt --check src/lib.rs"))
        self.assertIsNone(check_command("uv run --no-project soldr cargo test"))
        self.assertIsNone(check_command("uv run --no-sync soldr rustfmt --check src/lib.rs"))

    def test_blocks_bare_python(self):
        result = check_command("python ci/script.py")
        self.assertIsNotNone(result)
        self.assertEqual(result[0], "python")

    def test_blocks_bare_python3(self):
        result = check_command("python3 ci/script.py")
        self.assertIsNotNone(result)
        self.assertEqual(result[0], "python3")

    def test_blocks_unguarded_uv_run_script(self):
        for command in (
            "uv run script.py",
            "uv run python script.py",
            "uv run pytest tests",
            "uv run soldr cargo check",
        ):
            with self.subTest(command=command):
                result = check_command(command)
                self.assertIsNotNone(result)
                self.assertEqual(result[0], "uv run")
                self.assertIn("--no-project", result[1])

    def test_allows_guarded_uv_run_script(self):
        self.assertIsNone(check_command("uv run --no-project script.py"))
        self.assertIsNone(check_command("uv run --no-project python script.py"))
        self.assertIsNone(check_command("uv run --no-sync pytest tests"))
        self.assertIsNone(check_command("uv run --frozen pytest tests"))

    def test_repo_dev_loop_scripts_do_not_use_unsafe_uv_shapes(self):
        failures = []
        for name in DEV_LOOP_SCRIPTS:
            path = REPO_ROOT / name
            for line_number, raw_line in enumerate(path.read_text().splitlines(), 1):
                line = raw_line.strip()
                if not line or line.startswith("#"):
                    continue
                if "uv sync" in line:
                    failures.append(f"{name}:{line_number}: {line}")
                if "uv run" in line and not any(flag in line for flag in UV_RUN_GUARDS):
                    failures.append(f"{name}:{line_number}: {line}")
        self.assertEqual([], failures)

    def test_blocks_bare_pip(self):
        result = check_command("pip install foo")
        self.assertIsNotNone(result)
        self.assertEqual(result[0], "pip")
        # Suggestion should mention `uv pip install foo`.
        self.assertIn("uv pip install foo", result[1])

    def test_blocks_bare_pip3(self):
        result = check_command("pip3 install foo")
        self.assertIsNotNone(result)
        self.assertEqual(result[0], "pip3")

    def test_allows_uv_pip(self):
        self.assertIsNone(check_command("uv pip install foo"))
        self.assertIsNone(check_command("uv pip list"))

    def test_extracts_powershell_command_field(self):
        command = extract_command({
            "tool_name": "PowerShell",
            "tool_input": {"command": "cargo test"},
        })
        self.assertEqual(command, "cargo test")

    def test_extracts_shell_script_field(self):
        command = extract_command({
            "tool_name": "Shell",
            "tool_input": {"script": "cargo test"},
        })
        self.assertEqual(command, "cargo test")

    def test_accepts_codex_shell_tool_names(self):
        self.assertIn("shell_command", SHELL_TOOL_NAMES)
        self.assertIn("functions.shell_command", SHELL_TOOL_NAMES)

    def test_allows_unrelated_command(self):
        self.assertIsNone(check_command("ls -la"))
        self.assertIsNone(check_command("git status"))

    def test_blocks_compound_command(self):
        # A bare cargo invocation hidden behind && must still trip the guard.
        result = check_command("git status && cargo test")
        self.assertIsNotNone(result)
        self.assertEqual(result[0], "cargo")

    def test_blocks_bare_rustup(self):
        result = check_command("rustup target add x86_64-pc-windows-msvc")
        self.assertIsNotNone(result)
        self.assertEqual(result[0], "rustup")

    def test_blocks_bare_rustdoc(self):
        result = check_command("rustdoc --output target/doc src/lib.rs")
        self.assertIsNotNone(result)
        self.assertEqual(result[0], "rustdoc")

    def test_blocks_bare_rust_gdb_lldb_analyzer(self):
        for tool in ("rust-gdb", "rust-lldb", "rust-analyzer"):
            with self.subTest(tool=tool):
                result = check_command(f"{tool} --version")
                self.assertIsNotNone(result)
                self.assertEqual(result[0], tool)

    def test_allows_soldr_rustup_rustdoc(self):
        self.assertIsNone(check_command("soldr rustup target add x86_64-pc-windows-msvc"))
        self.assertIsNone(check_command("soldr rustdoc --output target/doc src/lib.rs"))
        self.assertIsNone(check_command("uv run --no-project soldr rustup show"))

    def test_blocks_env_prefixed_bare_cargo(self):
        # Leading env-var assignments are not a backdoor around the policy.
        cases = (
            "RUSTUP_TOOLCHAIN=1.94.1 cargo build",
            "FOO=bar BAZ=qux cargo test",
            "CARGO_PROFILE_RELEASE_DEBUG=2 CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO=packed cargo build",
            "CARGO_TARGET_DIR=/tmp/foo cargo check",
        )
        for command in cases:
            with self.subTest(command=command):
                result = check_command(command)
                self.assertIsNotNone(result)
                self.assertEqual(result[0], "cargo")

    def test_blocks_env_prefixed_bare_rustup(self):
        result = check_command("RUSTUP_HOME=/tmp/h rustup show")
        self.assertIsNotNone(result)
        self.assertEqual(result[0], "rustup")

    def test_allows_env_prefixed_soldr_invocation(self):
        # Env-var assignments before soldr are fine -- the policy is about
        # routing the *tool*, not forbidding env overrides.
        self.assertIsNone(check_command("SOLDR_TRUST_MODE=strict soldr cargo build"))
        self.assertIsNone(check_command("CARGO_BUILD_JOBS=4 soldr cargo test --release"))
        self.assertIsNone(check_command("FOO=bar uv run --no-project soldr cargo check"))

    def test_blocks_env_prefixed_bare_python(self):
        result = check_command("PYTHONPATH=/foo python ci/script.py")
        self.assertIsNotNone(result)
        self.assertEqual(result[0], "python")

    def test_blocks_env_prefixed_in_compound(self):
        # Env-prefixed bare cargo on the right side of && must still trip.
        result = check_command("git status && RUSTUP_TOOLCHAIN=foo cargo build")
        self.assertIsNotNone(result)
        self.assertEqual(result[0], "cargo")

    # ── Quoting respect ───────────────────────────────────────────────
    # The hook splits unquoted shell operators but must NOT split on
    # operators that live inside a quoted argument. Otherwise legitimate
    # wrapping commands (`bash -c '...'`, `docker run ... bash -c '...'`,
    # `ssh host '...'`) get false-positive blocked.

    def test_allows_bash_dash_c_single_quoted_cargo(self):
        # The inner `cargo build` is a quoted -c arg, not a host shell
        # command. The host invocation is `bash`, which is fine.
        self.assertIsNone(check_command("bash -c 'cargo build'"))

    def test_allows_bash_dash_c_double_quoted_cargo(self):
        self.assertIsNone(check_command('bash -c "cargo build"'))

    def test_allows_sh_dash_c_with_compound_cargo(self):
        # Even when the quoted script contains operators, the host
        # invocation is `sh -c '...'` — one segment, headed by `sh`.
        self.assertIsNone(check_command("sh -c 'cargo build && cargo test'"))

    def test_allows_docker_run_with_quoted_cargo(self):
        # Real motivating case: `docker run ... bash -c '... cargo ...'`
        # should not trip the host policy.
        self.assertIsNone(check_command(
            "docker run --rm -v /work:/w img bash -c 'cargo build --release'"
        ))

    def test_allows_docker_run_bare_cargo_after_image(self):
        # `docker run img cargo build` — cargo is an arg to docker, not
        # a host process. Whitelisting docker explicitly is not needed
        # because the first token is `docker`, which isn't in RUST_TOOLS.
        self.assertIsNone(check_command(
            "docker run --rm -v /work:/w img cargo build --release"
        ))

    def test_allows_ssh_with_remote_cargo(self):
        # `ssh host cargo build` — the cargo runs on the remote host.
        # First token is `ssh`, not in RUST_TOOLS.
        self.assertIsNone(check_command("ssh host cargo build"))

    def test_allows_echo_of_cargo_string(self):
        # `echo 'cargo build'` is text, not an invocation.
        self.assertIsNone(check_command("echo 'cargo build'"))

    def test_blocks_cargo_after_quoted_wrapper(self):
        # Mixing styles: a quoted wrapper segment followed by a real
        # bare cargo invocation must still trip on the second segment.
        result = check_command("bash -c 'echo ok' && cargo test")
        self.assertIsNotNone(result)
        self.assertEqual(result[0], "cargo")

    def test_blocks_cargo_when_operator_unquoted(self):
        # Unquoted `;` is still a segment separator: `echo hi ; cargo build`
        # exposes `cargo build` as a bare host command and must trip.
        result = check_command("echo hi ; cargo build")
        self.assertIsNotNone(result)
        self.assertEqual(result[0], "cargo")

    def test_blocks_cargo_with_trailing_background_amp(self):
        # `cargo build &` — `&` is a control operator but `cargo` is
        # still the head of the segment.
        result = check_command("cargo build &")
        self.assertIsNotNone(result)
        self.assertEqual(result[0], "cargo")

    def test_malformed_quoting_falls_back_safely(self):
        # An unclosed quote shouldn't crash the hook AND shouldn't open
        # a backdoor. The legacy regex fallback inspects every rough
        # segment so a bare cargo on either side of the broken quote
        # still trips.
        result = check_command("echo 'unclosed && cargo build")
        self.assertIsNotNone(result)
        self.assertEqual(result[0], "cargo")


if __name__ == "__main__":
    unittest.main()
