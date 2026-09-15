"""Stage auxiliary Cargo ``[[bin]]`` targets into a soldr-built wheel.

soldr#3239. maturin builds one artifact kind per wheel: a PyO3 extension *or*
a ``bindings = "bin"`` executable. A project that ships both used to wrap
maturin in a bespoke ``delegate-backend`` whose only job was to build the bin
and copy it into maturin's data directory. ``bundle-bins`` moves that job into
the native backend::

    [tool.soldr.pep517]
    bundle-bins = [
      { bin = "bosn", package = "bosn" },                # -> <dist>.data/scripts/
      { bin = "helper", dest = "platlib/bosn/_bin" },    # -> site-packages/bosn/_bin/
    ]

After maturin writes the extension wheel, each entry is built through
``soldr build`` with the same prepared environment, target, and profile, and
the executable is added to the wheel with a regenerated ``RECORD``.

``dest`` is ``<scheme>[/<subdir>]`` using the wheel install schemes that
maturin's ``data`` directory also uses: ``scripts`` (the default, installed
onto ``PATH``), ``platlib``, ``purelib``, ``data``, and ``headers``.

This module has no dependency on the backend in ``__init__.py`` so it can be
unit-tested in isolation; the backend supplies the command environment.
"""

import base64
import csv
import hashlib
import io
import json
import os
import re
import stat
import subprocess
import sys
import zipfile
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Optional, Sequence

BUNDLE_BINS_KEY = "bundle-bins"
DEFAULT_DEST = "scripts"
WHEEL_SCHEMES = ("scripts", "platlib", "purelib", "data", "headers")
_SECTION = "tool.soldr.pep517"
_ENTRY_KEYS = {"bin", "package", "dest"}
_BIN_NAME = re.compile(r"[A-Za-z0-9_][A-Za-z0-9_.-]*")


class BundleBinsError(RuntimeError):
    """A ``bundle-bins`` configuration or staging failure."""


@dataclass(frozen=True)
class BundleBin:
    """One Cargo bin target to stage into the wheel."""

    bin: str
    package: Optional[str] = None
    dest: str = DEFAULT_DEST


def read_bundle_bins(pyproject: Path) -> "list[BundleBin]":
    """Return the validated ``bundle-bins`` entries, or ``[]`` when absent."""
    try:
        text = pyproject.read_text(encoding="utf-8")
    except OSError:
        return []
    return [_parse_entry(index, raw) for index, raw in enumerate(_raw_entries(text))]


def _raw_entries(text: str) -> "list[Any]":
    try:
        # pylint: disable=import-outside-toplevel  # 3.10 has no tomllib
        import tomllib  # type: ignore[import-not-found]
    except ImportError:
        return _fallback_entries(text)
    try:
        value: Any = tomllib.loads(text)
    except ValueError:
        # maturin reports the authoritative TOML error during the build.
        return []
    for component in (*_SECTION.split("."), BUNDLE_BINS_KEY):
        if not isinstance(value, dict):
            return []
        value = value.get(component)
    if value is None:
        return []
    if not isinstance(value, list):
        raise BundleBinsError(
            f"[{_SECTION}] {BUNDLE_BINS_KEY} must be an array of tables"
        )
    return value


def _strip_comment(line: str) -> str:
    quote = None
    for index, char in enumerate(line):
        if quote:
            if char == quote:
                quote = None
        elif char in "\"'":
            quote = char
        elif char == "#":
            return line[:index]
    return line


def _bracket_depth(text: str) -> int:
    depth = 0
    quote = None
    for char in text:
        if quote:
            if char == quote:
                quote = None
        elif char in "\"'":
            quote = char
        elif char == "[":
            depth += 1
        elif char == "]":
            depth -= 1
    return depth


def _fallback_entries(text: str) -> "list[Any]":
    """Parse the one supported ``bundle-bins`` shape without tomllib.

    Python 3.10 has no tomllib and the backend must stay dependency-free, so
    this accepts exactly the documented form: an array of inline tables whose
    values are strings, optionally spread over several lines.
    """
    active = False
    collected: "list[str] | None" = None
    for line in text.splitlines():
        stripped = _strip_comment(line).strip()
        if collected is not None:
            collected.append(stripped)
            if _bracket_depth(" ".join(collected)) <= 0:
                break
            continue
        if stripped.startswith("["):
            active = re.sub(r"\s+", "", stripped) == f"[{_SECTION}]"
            continue
        key, separator, rest = stripped.partition("=")
        if active and separator and key.strip() == BUNDLE_BINS_KEY:
            collected = [rest.strip()]
            if _bracket_depth(rest) <= 0:
                break
    if collected is None:
        return []
    body = " ".join(collected).strip()
    if not (body.startswith("[") and body.endswith("]")):
        raise BundleBinsError(
            f"[{_SECTION}] {BUNDLE_BINS_KEY} must be an array of tables"
        )
    entries: "list[Any]" = []
    for table in re.findall(r"\{([^{}]*)\}", body):
        entry: dict[str, str] = {}
        for match in re.finditer(
            r"([A-Za-z0-9_-]+)\s*=\s*(?:\"([^\"]*)\"|'([^']*)')", table
        ):
            value = match.group(2) if match.group(2) is not None else match.group(3)
            entry[match.group(1)] = value
        entries.append(entry)
    return entries


def _parse_entry(index: int, raw: Any) -> BundleBin:
    where = f"[{_SECTION}] {BUNDLE_BINS_KEY}[{index}]"
    if not isinstance(raw, dict):
        raise BundleBinsError(f'{where} must be a table such as {{ bin = "name" }}')
    unknown = sorted(set(raw) - _ENTRY_KEYS)
    if unknown:
        raise BundleBinsError(
            f"{where} has unknown key(s) {', '.join(unknown)}; "
            f"supported keys are {', '.join(sorted(_ENTRY_KEYS))}"
        )
    name = raw.get("bin")
    if not isinstance(name, str) or not _BIN_NAME.fullmatch(name):
        raise BundleBinsError(f"{where} needs `bin`, the Cargo [[bin]] target name")
    package = raw.get("package")
    if package is not None and (not isinstance(package, str) or not package.strip()):
        raise BundleBinsError(f"{where} `package` must be a non-empty string")
    dest = raw.get("dest", DEFAULT_DEST)
    if not isinstance(dest, str):
        raise BundleBinsError(f"{where} `dest` must be a string")
    return BundleBin(
        bin=name,
        package=package.strip() if package else None,
        dest=_normalize_dest(where, dest),
    )


def _normalize_dest(where: str, dest: str) -> str:
    parts = dest.strip().split("/")
    if (
        "\\" in dest
        or any(part in ("", ".", "..") for part in parts)
        or parts[0] not in WHEEL_SCHEMES
    ):
        raise BundleBinsError(
            f"{where} `dest` must be <scheme>[/<subdir>] with scheme one of "
            f"{', '.join(WHEEL_SCHEMES)} and no empty, `.`, or `..` components; "
            f"got {dest!r}"
        )
    return "/".join(parts)


def cargo_build_command(
    entry: BundleBin,
    *,
    manifest_path: Optional[Path],
    profile_args: Sequence[str],
    target_args: Sequence[str],
) -> "list[str]":
    """Build one bin through soldr's blessed build surface."""
    command = [
        "soldr",
        "build",
        "--bin",
        entry.bin,
        "--message-format=json-render-diagnostics",
    ]
    if entry.package:
        command += ["--package", entry.package]
    if manifest_path is not None:
        command += ["--manifest-path", str(manifest_path)]
    return [*command, *profile_args, *target_args]


def executable_from_messages(stdout: str, bin_name: str) -> Path:
    """Find the bin's executable in Cargo's JSON message stream."""
    found: Optional[Path] = None
    for line in stdout.splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            message = json.loads(line)
        except ValueError:
            continue
        if (
            not isinstance(message, dict)
            or message.get("reason") != "compiler-artifact"
        ):
            continue
        target = message.get("target")
        if not isinstance(target, dict) or target.get("name") != bin_name:
            continue
        if "bin" not in (target.get("kind") or []):
            continue
        executable = message.get("executable")
        if isinstance(executable, str) and executable:
            found = Path(executable)
    if found is None:
        raise BundleBinsError(
            f"cargo reported no executable for bundled bin `{bin_name}`"
        )
    return found


def build_bundle_bin(
    entry: BundleBin,
    command: Sequence[str],
    env: "dict[str, str]",
    run: Callable[..., Any] = subprocess.run,
) -> Path:
    """Run ``command`` and return the built executable for ``entry``."""
    result = run(
        list(command),
        env=env,
        stdout=subprocess.PIPE,
        text=True,
        encoding="utf-8",
        errors="replace",
        check=False,
    )
    stdout = result.stdout or ""
    # Cargo's rendered diagnostics and progress already go to stderr; relay
    # any non-JSON stdout too so the frontend shows the whole build.
    for line in stdout.splitlines():
        if line.strip() and not line.lstrip().startswith("{"):
            print(line, file=sys.stderr)
    if result.returncode != 0:
        raise BundleBinsError(
            f"`{' '.join(command)}` exited with {result.returncode} while building "
            f"bundled bin `{entry.bin}`"
        )
    executable = executable_from_messages(stdout, entry.bin)
    if not executable.is_file():
        raise BundleBinsError(
            f"bundled bin `{entry.bin}` was reported at {executable}, which is not a file"
        )
    return executable


def wheel_arcname(prefix: str, dest: str, filename: str, root_is_purelib: bool) -> str:
    """Map an install scheme destination to its path inside the wheel."""
    scheme, _, subdir = dest.partition("/")
    parts = [part for part in (subdir, filename) if part]
    if (scheme == "platlib" and not root_is_purelib) or (
        scheme == "purelib" and root_is_purelib
    ):
        return "/".join(parts)
    return "/".join([f"{prefix}.data", scheme, *parts])


def _record_row(name: str, data: bytes) -> "list[str]":
    digest = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=")
    return [name, f"sha256={digest.decode('ascii')}", str(len(data))]


def _dist_info_directory(names: "Sequence[str]") -> str:
    candidates = sorted(
        {
            name.split("/", 1)[0]
            for name in names
            if name.count("/") == 1
            and name.split("/", 1)[0].endswith(".dist-info")
            and name.endswith("/RECORD")
        }
    )
    if len(candidates) != 1:
        raise BundleBinsError(
            f"expected exactly one top-level *.dist-info/RECORD in the wheel, found {candidates}"
        )
    return candidates[0]


def _root_is_purelib(wheel_metadata: str) -> bool:
    for line in wheel_metadata.splitlines():
        key, separator, value = line.partition(":")
        if separator and key.strip().lower() == "root-is-purelib":
            return value.strip().lower() == "true"
    return False


def add_files_to_wheel(
    wheel: Path, files: "Sequence[tuple[BundleBin, Path]]"
) -> "list[str]":
    """Add executables to ``wheel`` in place and regenerate its ``RECORD``.

    Returns the wheel paths that were added.
    """
    with zipfile.ZipFile(wheel) as source:
        infos = source.infolist()
        names = [info.filename for info in infos]
        dist_info = _dist_info_directory(names)
        record_name = f"{dist_info}/RECORD"
        record_info = source.getinfo(record_name)
        root_is_purelib = _root_is_purelib(
            source.read(f"{dist_info}/WHEEL").decode("utf-8", errors="replace")
        )
        prefix = dist_info[: -len(".dist-info")]
        additions: "list[tuple[str, Path]]" = []
        taken = set(names)
        for entry, executable in files:
            arcname = wheel_arcname(
                prefix, entry.dest, executable.name, root_is_purelib
            )
            if arcname in taken:
                raise BundleBinsError(
                    f"bundled bin `{entry.bin}` would overwrite `{arcname}` already in the wheel"
                )
            taken.add(arcname)
            additions.append((arcname, executable))

        temporary = wheel.with_name(f".{wheel.name}.{os.getpid()}.tmp")
        rows: "list[list[str]]" = []
        try:
            with zipfile.ZipFile(
                temporary, "w", compression=zipfile.ZIP_DEFLATED
            ) as out:
                for info in infos:
                    if info.filename == record_name:
                        continue
                    data = source.read(info)
                    out.writestr(info, data)
                    if not info.is_dir():
                        rows.append(_record_row(info.filename, data))
                for arcname, executable in additions:
                    data = executable.read_bytes()
                    # Reuse RECORD's timestamp so the added entries are as
                    # reproducible as the wheel maturin produced.
                    info = zipfile.ZipInfo(arcname, date_time=record_info.date_time)
                    info.external_attr = (stat.S_IFREG | 0o755) << 16
                    info.compress_type = zipfile.ZIP_DEFLATED
                    out.writestr(info, data)
                    rows.append(_record_row(arcname, data))
                rows.append([record_name, "", ""])
                buffer = io.StringIO()
                csv.writer(buffer, lineterminator="\n").writerows(rows)
                out.writestr(record_info, buffer.getvalue())
        except BaseException:
            temporary.unlink(missing_ok=True)
            raise
    os.replace(temporary, wheel)
    return [arcname for arcname, _ in additions]


def bundle_into_wheel(
    wheel: Path,
    entries: "Sequence[BundleBin]",
    build: Callable[[BundleBin], Path],
) -> "list[str]":
    """Build every entry, then stage all of them into ``wheel`` at once."""
    built = [(entry, build(entry)) for entry in entries]
    return add_files_to_wheel(wheel, built)
