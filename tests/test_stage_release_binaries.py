"""Tests for the extracted release binary-staging gate (soldr#2469)."""

from __future__ import annotations

from pathlib import Path

import pytest
from conftest import load_script_module

REPO_ROOT = Path(__file__).parents[1]
SCRIPTS = REPO_ROOT / ".github" / "scripts"
WORKFLOW = REPO_ROOT / ".github" / "workflows" / "release-auto.yml"

stage = load_script_module(
    SCRIPTS / "stage_release_binaries.py", "stage_release_binaries"
)


def write_file(directory: Path, name: str, contents: bytes = b"artifact") -> Path:
    path = directory / name
    path.write_bytes(contents)
    return path


def test_windows_requires_and_stages_a_pdb_sidecar(tmp_path: Path) -> None:
    release = tmp_path / "release"
    package = tmp_path / "package"
    release.mkdir()
    write_file(release, "soldr.exe", b"soldr")
    write_file(release, "soldr_cli.pdb", b"symbols")

    staged = stage.stage_release_binaries("x86_64-pc-windows-msvc", release, package)

    assert [path.name for path in staged] == [
        "soldr.exe",
        "soldr-daemon.exe",
        "soldr_cli.pdb",
    ]
    assert (package / "soldr.exe").read_bytes() == b"soldr"
    assert (package / "soldr-daemon.exe").read_bytes() == b"soldr"
    assert (package / "soldr_cli.pdb").read_bytes() == b"symbols"


def test_windows_missing_pdb_is_a_named_release_failure(tmp_path: Path) -> None:
    release = tmp_path / "release"
    release.mkdir()
    write_file(release, "soldr.exe")

    with pytest.raises(stage.StagingError, match="PDB sidecar"):
        stage.stage_release_binaries(
            "x86_64-pc-windows-msvc", release, tmp_path / "package"
        )


def test_linux_release_with_split_dwarf_never_stages_it_into_package_dir(
    tmp_path: Path,
) -> None:
    """soldr#3038 regression guard: package_dir feeds manifest.json's
    soldr.debug_info, which every setup-soldr consumer requires to be
    pdb-only (zccache_contract.py::validate_release_manifest). A .dwp must
    never land here even when the release profile produced one.
    """
    release = tmp_path / "release"
    package = tmp_path / "package"
    release.mkdir()
    write_file(release, "soldr")
    write_file(release, "soldr_cli.dwp", b"debug")

    staged = stage.stage_release_binaries("x86_64-unknown-linux-gnu", release, package)

    assert [path.name for path in staged] == ["soldr", "soldr-daemon"]
    assert not (package / "soldr_cli.dwp").exists()


def test_linux_release_without_split_dwarf_still_stages_binary(tmp_path: Path) -> None:
    release = tmp_path / "release"
    package = tmp_path / "package"
    release.mkdir()
    write_file(release, "soldr")

    staged = stage.stage_release_binaries("x86_64-unknown-linux-musl", release, package)

    assert [path.name for path in staged] == ["soldr", "soldr-daemon"]


def test_macos_release_with_dsym_never_stages_it_into_package_dir(
    tmp_path: Path,
) -> None:
    """soldr#3038 regression guard, macOS half of the Linux .dwp test above."""
    release = tmp_path / "release"
    package = tmp_path / "package"
    dsym = release / "soldr.dSYM" / "Contents" / "Resources"
    dsym.mkdir(parents=True)
    write_file(release, "soldr")
    write_file(dsym, "DWARF", b"symbols")

    staged = stage.stage_release_binaries("aarch64-apple-darwin", release, package)

    assert [path.name for path in staged] == ["soldr", "soldr-daemon"]
    assert not (package / "soldr.dSYM").exists()


def fake_objcopy_strip(calls: list[list[str]]):
    """A stand-in for GNU objcopy/strip that records invocations and mutates
    files the same shape the real tools would, without needing a real ELF
    binary or the tools themselves installed in the test environment.
    """

    def run_tool(args: list[str]) -> None:
        calls.append(list(args))
        program = Path(args[0]).name
        if program in {"objcopy", "llvm-objcopy"} and args[1] == "--only-keep-debug":
            # objcopy --only-keep-debug SRC DEST
            Path(args[3]).write_bytes(b"debug-section-bytes")
        elif program in {"strip", "llvm-strip"} and args[1] == "--strip-debug":
            # strip --strip-debug BINARY -- modifies BINARY's own inode in
            # place, exactly like the real tool: a hardlinked soldr-daemon
            # sees this too, for free.
            Path(args[2]).write_bytes(b"stripped-binary-bytes")
        elif program in {"objcopy", "llvm-objcopy"} and args[1].startswith(
            "--add-gnu-debuglink"
        ):
            pass  # in-place section add; no observable content change here
        elif program in {"strip", "llvm-strip"} and args[1] == "-x":
            # macOS: strip -x BINARY, same in-place-mutation shape.
            Path(args[2]).write_bytes(b"darwin-stripped-bytes")
        else:  # pragma: no cover - guards against an un-mocked call shape
            raise AssertionError(f"unexpected tool invocation: {args}")

    return run_tool


def test_stage_debug_symbols_carves_linux_debug_info_via_objcopy(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    release = tmp_path / "release"
    package = tmp_path / "package"
    symbols = tmp_path / "symbols"
    release.mkdir()
    write_file(release, "soldr", b"linked-binary-bytes")
    stage.stage_release_binaries("x86_64-unknown-linux-gnu", release, package)

    calls: list[list[str]] = []
    monkeypatch.setattr(stage, "run_tool", fake_objcopy_strip(calls))

    staged = stage.stage_debug_symbols(
        "x86_64-unknown-linux-gnu", release, package, symbols
    )

    assert [path.name for path in staged] == ["soldr.debug"]
    assert (symbols / "soldr.debug").read_bytes() == b"debug-section-bytes"
    debug_dest = str(symbols / "soldr.debug")
    soldr = str(package / "soldr")
    assert calls == [
        ["objcopy", "--only-keep-debug", soldr, debug_dest],
        ["strip", "--strip-debug", soldr],
        ["objcopy", f"--add-gnu-debuglink={debug_dest}", soldr],
    ]
    # soldr-daemon was staged as a hardlink to soldr (same inode); the fake
    # strip's in-place write to soldr's inode is visible through it too, for
    # free -- matching the real tool's behavior. No re-derive needed.
    assert (package / "soldr-daemon").read_bytes() == b"stripped-binary-bytes"


def test_stage_debug_symbols_re_derives_a_daemon_that_was_not_hardlinked(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """soldr#3038: 'verify by hash rather than assuming.'

    If `soldr-daemon` ever reaches this point as an independent copy (the
    `copy_or_link` cross-device fallback) rather than a hardlink, stripping
    `soldr` in place does NOT touch it -- it would ship with stale,
    unstripped bytes unless this notices the mismatch and re-derives it.
    """
    release = tmp_path / "release"
    package = tmp_path / "package"
    symbols = tmp_path / "symbols"
    release.mkdir()
    write_file(release, "soldr", b"linked-binary-bytes")
    stage.stage_release_binaries("x86_64-unknown-linux-gnu", release, package)

    # Simulate the copy fallback: replace the hardlinked daemon with an
    # independent file holding different (stale) bytes.
    (package / "soldr-daemon").unlink()
    (package / "soldr-daemon").write_bytes(b"stale-independent-copy")

    calls: list[list[str]] = []
    monkeypatch.setattr(stage, "run_tool", fake_objcopy_strip(calls))

    stage.stage_debug_symbols("x86_64-unknown-linux-gnu", release, package, symbols)

    assert (package / "soldr-daemon").read_bytes() == (package / "soldr").read_bytes()
    assert (package / "soldr-daemon").read_bytes() == b"stripped-binary-bytes"


def test_stage_debug_symbols_requires_the_binary_already_staged(
    tmp_path: Path,
) -> None:
    release = tmp_path / "release"
    package = tmp_path / "package"  # deliberately never staged
    symbols = tmp_path / "symbols"
    release.mkdir()
    package.mkdir()

    with pytest.raises(stage.StagingError, match="already staged"):
        stage.stage_debug_symbols("x86_64-unknown-linux-gnu", release, package, symbols)


def test_run_tool_wraps_a_failing_command_in_a_staging_error() -> None:
    import sys

    with pytest.raises(stage.StagingError, match="exit 3"):
        stage.run_tool([sys.executable, "-c", "import sys; sys.exit(3)"])


def test_stage_debug_symbols_stages_macos_dsym_and_strips_the_staged_binary(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    release = tmp_path / "release"
    package = tmp_path / "package"
    symbols = tmp_path / "symbols"
    dsym = release / "soldr.dSYM" / "Contents" / "Resources"
    dsym.mkdir(parents=True)
    write_file(release, "soldr")
    write_file(dsym, "DWARF", b"symbols")
    stage.stage_release_binaries("aarch64-apple-darwin", release, package)

    calls: list[list[str]] = []
    monkeypatch.setattr(stage, "run_tool", fake_objcopy_strip(calls))

    staged = stage.stage_debug_symbols(
        "aarch64-apple-darwin", release, package, symbols
    )

    assert [path.name for path in staged] == ["soldr.dSYM"]
    assert (
        symbols / "soldr.dSYM" / "Contents" / "Resources" / "DWARF"
    ).read_bytes() == b"symbols"
    # dsymutil already captured everything from the intermediate .o files
    # (see the module docstring); `strip -x` runs on the STAGED binary
    # afterward, and the hardlinked daemon reflects it for free.
    assert calls == [["strip", "-x", str(package / "soldr")]]
    assert (package / "soldr").read_bytes() == b"darwin-stripped-bytes"
    assert (package / "soldr-daemon").read_bytes() == b"darwin-stripped-bytes"


def test_stage_debug_symbols_is_a_clean_no_op_when_nothing_was_emitted(
    tmp_path: Path,
) -> None:
    """Windows always, and any Unix build whose profile emitted no sidecar."""
    release = tmp_path / "release"
    package = tmp_path / "package"
    symbols = tmp_path / "symbols"
    release.mkdir()
    write_file(release, "soldr.exe")
    write_file(release, "soldr_cli.pdb", b"pdb-bytes")
    stage.stage_release_binaries("x86_64-pc-windows-msvc", release, package)

    staged = stage.stage_debug_symbols(
        "x86_64-pc-windows-msvc", release, package, symbols
    )

    assert staged == []
    assert not symbols.exists()


def test_missing_main_binary_reports_observed_release_directory(tmp_path: Path) -> None:
    release = tmp_path / "release"
    release.mkdir()
    write_file(release, "another-file")

    with pytest.raises(stage.StagingError, match="expected soldr") as error:
        stage.stage_release_binaries(
            "x86_64-unknown-linux-gnu", release, tmp_path / "package"
        )

    assert "another-file" in str(error.value)


def test_workflow_invokes_the_script_instead_of_inlining_binary_staging() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    assert ".github/scripts/stage_release_binaries.py" in workflow
    assert "pdb_src=" not in workflow
    assert "staged Linux split-DWARF sidecar" not in workflow


def test_daemon_sidecar_links_through_a_portable_api() -> None:
    """`Path.hardlink_to` is Python 3.10+; the release runners are not all.

    A runner whose `python3` predated 3.10 raised AttributeError here and took
    down the v0.9.3 macOS ARM64 release lane (soldr#2763). A behavioral test
    cannot catch a reintroduction, because the interpreter running the suite
    has the newer API -- so assert the source-level choice instead.
    """
    source = (SCRIPTS / "stage_release_binaries.py").read_text(encoding="utf-8")
    assert "os.link(" in source
    # The call form, not the bare name -- the code comment above the fix
    # names the rejected API deliberately.
    assert ".hardlink_to(" not in source


def test_release_jobs_running_python_pin_the_interpreter() -> None:
    """Every release job that shells to python3 must install a known one."""
    workflow = WORKFLOW.read_text(encoding="utf-8")

    # The build matrix stages binaries; the macOS ARM smoke runs
    # ci/smoke_release_artifacts.py. Both are macOS-capable lanes that
    # previously inherited the image's interpreter.
    assert workflow.count('python-version: "3.13"') >= 3


def test_daemon_sidecar_is_staged_from_the_release_binary(tmp_path: Path) -> None:
    """Guard the link/copy fallback itself, not just which API it names."""
    release = tmp_path / "release"
    package = tmp_path / "package"
    release.mkdir()
    write_file(release, "soldr", b"soldr-bytes")

    stage.stage_release_binaries("x86_64-unknown-linux-gnu", release, package)

    assert (package / "soldr-daemon").read_bytes() == b"soldr-bytes"
