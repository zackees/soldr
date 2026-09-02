#!/usr/bin/env python3
"""Stage Soldr's executable and debug sidecars for a release archive.

This is the release workflow's narrow packaging boundary: it copies the built
``soldr`` executable to ``dist/package``, creates the ``soldr-daemon``
sidecar, and (Windows only) stages the required PDB. ``package_dir`` is
exactly what ``release_manifest.py`` and the main ``.tar.zst`` archive step
consume, so a missing Windows PDB must fail here.

soldr#3038: Linux debug info and macOS ``.dSYM`` sidecars are staged by
:func:`stage_debug_symbols` into a SEPARATE directory, never into
``package_dir``. That tree is what ``release_manifest.py::collect_debug_info``
reads for ``soldr.debug_info``, and every ``setup-soldr`` consumer runs
``.github/actions/setup-soldr/zccache_contract.py::validate_release_manifest``
against that field, which hard-rejects any entry whose ``format`` is not
``"pdb"``. See ``docs/DEBUG_SIDECARS.md`` for the resulting contract: symbols
ship as their own opt-in ``-symbols.tar.zst`` release asset that
``setup-soldr`` never downloads or parses.

soldr#3038 also replaced rustc's ``split-debuginfo = "packed"`` with a
post-link ``objcopy``/``strip`` carve-out for Linux (both gnu and musl):
"packed" only captures DWARF for the crates rustc itself compiles in this
build, not the precompiled std or the C dependencies built through the `cc`
crate (`ring`, `zstd-sys`, `lzma-sys`, `libsqlite3-sys`, mimalloc-pprof),
which embed their own DWARF in the final link regardless of that setting. The
result was duplication, not splitting: soldr's shipped binary went 21.3 MiB
(stripped) -> 73.8 MiB ("packed") while the `.dwp` beside it held another
48.5 MiB. Running `objcopy --only-keep-debug` on the fully linked binary
after the fact captures every source of DWARF in one pass with nothing left
behind. macOS keeps `split-debuginfo = "packed"` (dsymutil's `.dSYM` model
does not have this duplication problem) with a `strip -x` pass afterward;
Windows is unaffected (MSVC already emits a separate, required PDB).

It was extracted from release-auto.yml as part of soldr#2469 step 2.2 so the
platform-specific staging policy can be unit-tested without running a release.

Usage (CI):
    python3 .github/scripts/stage_release_binaries.py \
        --target x86_64-unknown-linux-gnu --package-dir dist/package \
        --symbols-dir dist/symbols
"""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
from pathlib import Path

from release_artifacts import binary_suffix


class StagingError(RuntimeError):
    """A required release artifact was absent or could not be staged."""


def release_contents(directory: Path) -> str:
    if not directory.is_dir():
        return f"{directory} does not exist"
    entries = sorted(path.name for path in directory.iterdir())
    return "\n".join(f"  {entry}" for entry in entries) or "  (empty)"


def first_file(directory: Path, names: list[str]) -> Path | None:
    return next(
        (directory / name for name in names if (directory / name).is_file()), None
    )


def first_directory(directory: Path, names: list[str]) -> Path | None:
    return next(
        (directory / name for name in names if (directory / name).is_dir()), None
    )


def files_equal(left: Path, right: Path) -> bool:
    """Byte-for-byte comparison.

    Used to VERIFY (soldr#3038 asked for this explicitly, rather than
    assuming) whether ``soldr-daemon`` is still byte-identical to ``soldr``
    after the post-link strip below. ``copy_or_link`` prefers a hardlink, in
    which case stripping ``soldr`` in place already strips ``soldr-daemon``
    for free (same inode). The cross-device copy fallback does not share
    that property, so this check -- not an assumption about which path
    `copy_or_link` took -- decides whether `soldr-daemon` needs re-deriving.
    """
    if not left.is_file() or not right.is_file():
        return False
    if left.stat().st_size != right.stat().st_size:
        return False
    with left.open("rb") as lf, right.open("rb") as rf:
        while True:
            lchunk = lf.read(1024 * 1024)
            rchunk = rf.read(1024 * 1024)
            if lchunk != rchunk:
                return False
            if not lchunk:
                return True


def copy_or_link(source: Path, destination: Path) -> None:
    """Prefer a hardlink for the daemon sidecar, then preserve portability."""
    # os.link rather than Path.hardlink_to: the latter is Python 3.10+, and a
    # release runner that shipped an older python3 broke the v0.9.3 macOS
    # ARM64 lane with AttributeError (soldr#2763). The workflow now pins the
    # interpreter, but the staging boundary must not depend on that to be
    # correct. Note the argument order inverts: os.link(src, dst) creates dst.
    #
    # If `destination` already exists (soldr#3038's stage_debug_symbols
    # re-derives it after a strip), `os.link` raises FileExistsError -- an
    # OSError -- so this still lands on the `shutil.copy2` fallback, which
    # overwrites in place. Correct either way; only the hardlink space
    # saving is lost on a re-derive.
    try:
        os.link(source, destination)
    except OSError:
        shutil.copy2(source, destination)


def mark_executable(path: Path) -> None:
    try:
        path.chmod(path.stat().st_mode | 0o755)
    except OSError:
        # The Windows executable bit is irrelevant, and archive creation still
        # carries the regular file.  Preserve the old workflow's best-effort
        # chmod behavior on hosts that reject the mode change.
        pass


def run_tool(args: list[str]) -> None:
    """Run one objcopy/strip step, folding output into a StagingError."""
    completed = subprocess.run(args, capture_output=True, text=True, check=False)
    if completed.returncode != 0:
        raise StagingError(
            f"{' '.join(args)} failed (exit {completed.returncode}):\n"
            f"{completed.stdout}{completed.stderr}"
        )


def strip_elf_binary_with_debuglink(
    binary: Path, debug_dest: Path, *, objcopy: str, strip_tool: str
) -> None:
    """Post-link symbol carve-out for one ELF binary (soldr#3038).

    See the module docstring for why this replaced
    `split-debuginfo = "packed"` on Linux. Three steps:

      1. `objcopy --only-keep-debug binary debug_dest` -- copy every debug
         section out to its own file. Operates on the COMPLETE linked
         binary, so it captures debug info from every source: soldr's own
         crates, the precompiled std, and every `cc`-built C dependency.
      2. `strip --strip-debug binary` -- remove the debug sections from the
         shipped binary in place. This is `--strip-debug`, not
         `--strip-all`: the GNU build-id note this release lane already
         injects (`-C link-arg=-Wl,--build-id`) lives in `.note.gnu.build-id`,
         not a `.debug_*` section, so it survives untouched -- a symbolizer
         can still cross-check binary and `.debug` file by build-id, not
         only by the debuglink CRC below.
      3. `objcopy --add-gnu-debuglink=debug_dest binary` -- embed a
         `.gnu_debuglink` section (the sidecar's basename plus a CRC32 of
         its contents) so a debugger/symbolizer that has both files locates
         one from the other without needing the build-id path at all.
    """
    run_tool([objcopy, "--only-keep-debug", str(binary), str(debug_dest)])
    run_tool([strip_tool, "--strip-debug", str(binary)])
    run_tool([objcopy, f"--add-gnu-debuglink={debug_dest}", str(binary)])


def stage_release_binaries(
    target: str, release_dir: Path, package_dir: Path
) -> list[Path]:
    """Populate ``package_dir`` with the executable, daemon, and (Windows) PDB.

    Never stages a macOS ``.dSYM`` and never carries an already-split Linux
    debug sidecar -- see the module docstring and :func:`stage_debug_symbols`,
    which strips the Linux binaries staged here in place as a second pass.
    """
    suffix = binary_suffix(target)
    source = release_dir / f"soldr{suffix}"
    if not source.is_file():
        raise StagingError(
            f"expected soldr{suffix} in target dir; observed release dir:\n"
            f"{release_contents(release_dir)}"
        )

    package_dir.mkdir(parents=True, exist_ok=True)
    staged: list[Path] = []
    soldr = package_dir / source.name
    shutil.copy2(source, soldr)
    mark_executable(soldr)
    staged.append(soldr)

    daemon = package_dir / f"soldr-daemon{suffix}"
    copy_or_link(soldr, daemon)
    mark_executable(daemon)
    staged.append(daemon)

    if target.endswith("-pc-windows-msvc"):
        pdb = first_file(release_dir, ["soldr.pdb", "soldr_cli.pdb"])
        if pdb is None:
            raise StagingError(
                f"expected a soldr PDB sidecar next to soldr{suffix}; observed release dir:\n"
                f"{release_contents(release_dir)}"
            )
        destination = package_dir / pdb.name
        shutil.copy2(pdb, destination)
        staged.append(destination)

    print("--- staged release package ---")
    print(release_contents(package_dir))
    return staged


def stage_debug_symbols(
    target: str,
    release_dir: Path,
    package_dir: Path,
    symbols_dir: Path,
    *,
    objcopy: str = "objcopy",
    strip_tool: str = "strip",
    darwin_strip: str = "strip",
) -> list[Path]:
    """Carve debug symbols out of the ALREADY-STAGED binaries in
    ``package_dir`` into their own directory, SEPARATE from ``package_dir``
    itself (soldr#3038).

    This must never write a non-pdb sidecar into ``package_dir``:
    ``release_manifest.py`` reads that tree for ``soldr.debug_info``, and
    every ``setup-soldr`` consumer's
    ``zccache_contract.py::validate_release_manifest`` hard-rejects any entry
    there whose ``format`` is not ``"pdb"``. Instead this directory becomes
    its own opt-in ``-symbols.tar.zst`` release asset that ``setup-soldr``
    never downloads or parses.

    Requires :func:`stage_release_binaries` to have already populated
    ``package_dir`` -- the Linux path strips those staged binaries in place.

    Returns an empty list -- not an error -- when there is nothing to stage
    for this target (Windows always; a Unix build whose macOS dSYM never
    materialized).
    """
    staged: list[Path] = []
    suffix = binary_suffix(target)
    if "-unknown-linux-" in target:
        binary = package_dir / f"soldr{suffix}"
        if not binary.is_file():
            raise StagingError(
                f"stage_debug_symbols requires soldr{suffix} already staged in "
                f"{package_dir} -- call stage_release_binaries first"
            )
        symbols_dir.mkdir(parents=True, exist_ok=True)
        debug_dest = symbols_dir / "soldr.debug"
        strip_elf_binary_with_debuglink(
            binary, debug_dest, objcopy=objcopy, strip_tool=strip_tool
        )
        staged.append(debug_dest)
        print(
            f"carved debug info out of {binary.name} into {debug_dest.name} "
            f"(objcopy={objcopy!r}, strip={strip_tool!r})"
        )

        daemon = package_dir / f"soldr-daemon{suffix}"
        if daemon.is_file() and not files_equal(binary, daemon):
            copy_or_link(binary, daemon)
            mark_executable(daemon)
            print(
                f"{daemon.name} was not byte-identical to the stripped "
                f"{binary.name}; re-derived it"
            )
        elif daemon.is_file():
            print(
                f"{daemon.name} is already byte-identical to {binary.name} "
                "(shared inode reflected the strip)"
            )
    elif target.endswith("-apple-darwin"):
        dsym = first_directory(release_dir, ["soldr.dSYM", "soldr_cli.dSYM"])
        if dsym is not None:
            symbols_dir.mkdir(parents=True, exist_ok=True)
            destination = symbols_dir / dsym.name
            shutil.copytree(dsym, destination)
            staged.append(destination)
            print(f"staged macOS dSYM bundle into separate symbols asset: {dsym.name}")

            # dsymutil (run during the build by `split-debuginfo = "packed"`,
            # injected for darwin targets only -- see release-auto.yml /
            # native_release_build.py) gathers DWARF from the intermediate
            # `.o` files via N_OSO stabs, not from the linked binary's own
            # embedded debug info, so the dSYM above is already complete.
            # `strip -x` removes the local symbol table entries (those same
            # N_OSO stabs) the shipped binary no longer needs them for.
            binary = package_dir / "soldr"
            daemon = package_dir / "soldr-daemon"
            if binary.is_file():
                run_tool([darwin_strip, "-x", str(binary)])
                if daemon.is_file() and not files_equal(binary, daemon):
                    copy_or_link(binary, daemon)
                    mark_executable(daemon)

    if staged:
        print("--- staged debug-symbols package ---")
        print(release_contents(symbols_dir))
    return staged


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True)
    parser.add_argument("--package-dir", type=Path, default=Path("dist/package"))
    parser.add_argument("--release-dir", type=Path)
    parser.add_argument(
        "--symbols-dir",
        type=Path,
        default=None,
        help=(
            "Optional. When given, also carve debug symbols out of the "
            "staged Linux binaries (or stage the macOS .dSYM) here -- NEVER "
            "into --package-dir. See stage_debug_symbols()."
        ),
    )
    parser.add_argument(
        "--objcopy",
        default="objcopy",
        help="objcopy executable for the Linux symbol carve-out (default: GNU objcopy on PATH).",
    )
    parser.add_argument(
        "--strip-tool",
        default="strip",
        help="strip executable for the Linux symbol carve-out (default: GNU strip on PATH).",
    )
    parser.add_argument(
        "--darwin-strip",
        default="strip",
        help="strip executable for the macOS post-dsymutil pass (default: strip on PATH).",
    )
    args = parser.parse_args(argv)
    release_dir = args.release_dir or Path("target") / args.target / "release"
    try:
        stage_release_binaries(args.target, release_dir, args.package_dir)
        if args.symbols_dir is not None:
            stage_debug_symbols(
                args.target,
                release_dir,
                args.package_dir,
                args.symbols_dir,
                objcopy=args.objcopy,
                strip_tool=args.strip_tool,
                darwin_strip=args.darwin_strip,
            )
    except (OSError, StagingError) as error:
        print(str(error), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
