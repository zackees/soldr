#!/usr/bin/env python3
"""Install exact target-native tools from the hash-pinned toolchain catalogue."""

from __future__ import annotations

import argparse
import hashlib
import http.client
import json
import os
import shutil
import subprocess
import tarfile
import tempfile
import time
import urllib.error
import urllib.request
import zipfile
from pathlib import Path, PurePosixPath
from typing import Any, Callable, TypeVar

# Reassembly is shared with the other catalogue consumers rather than
# copied here -- three scripts read this data and three copies would drift.
from toolchain_asset_query import write_multipart_asset  # noqa: E402

DEFAULT_CATALOGUE_URL = "https://zackees.github.io/soldr-toolchain/catalogue.v2.json"
CATALOGUE_SCHEMA_VERSION = 2

SUPPORTED_TARGETS = {
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
}
DOWNLOAD_TIMEOUT_SECS = 120
SMOKE_TIMEOUT_SECS = 30
DOWNLOAD_ATTEMPTS = 3
RETRY_BASE_DELAY_SECS = 0.5

T = TypeVar("T")


def retry_network(action: Callable[[], T], *, label: str) -> T:
    """Retry bounded transient I/O failures without weakening verification."""
    for attempt in range(1, DOWNLOAD_ATTEMPTS + 1):
        try:
            return action()
        except (
            urllib.error.URLError,
            TimeoutError,
            ConnectionError,
            http.client.IncompleteRead,
        ) as error:
            if attempt == DOWNLOAD_ATTEMPTS:
                raise SystemExit(
                    f"{label} failed after {DOWNLOAD_ATTEMPTS} attempts: {error}"
                ) from error
            time.sleep(RETRY_BASE_DELAY_SECS * (2 ** (attempt - 1)))
    raise AssertionError("retry loop exhausted without returning or raising")


def asset_name(tool: str, version: str, target: str) -> str:
    if target not in SUPPORTED_TARGETS:
        raise SystemExit(f"unsupported catalogue target: {target}")
    bare_version = version.removeprefix("v")
    return f"{tool}-{bare_version}-{target}.tar.gz"


def valid_sha256(value: object) -> bool:
    digest = str(value).lower()
    return len(digest) == 64 and all(char in "0123456789abcdef" for char in digest)


def select_entry(
    catalogue: dict[str, Any], *, tool: str, version: str, target: str
) -> dict[str, Any]:
    expected = asset_name(tool, version, target)
    entries = catalogue.get("entries")
    if not isinstance(entries, list):
        raise SystemExit("catalogue has no entries list")
    matches = [entry for entry in entries if entry.get("asset") == expected]
    if len(matches) != 1:
        raise SystemExit(
            f"catalogue must contain exactly one {expected} row; found {len(matches)}"
        )
    entry = matches[0]
    if not valid_sha256(entry.get("sha256")):
        raise SystemExit(f"catalogue row {expected} has no valid sha256")
    # A v2 row carries EITHER direct urls OR parts. Requiring `url` here is what
    # this script used to do, and it is wrong against v2: of the 152 published
    # rows, 150 are multipart and carry no single URL at all.
    if not direct_urls(entry) and not multipart_parts(entry):
        raise SystemExit(
            f"catalogue row {expected} has neither a download URL nor parts"
        )
    return entry


def direct_urls(entry: dict[str, Any]) -> list[str]:
    """Single-request download locations, newest spelling first.

    `urls` is the v2 form; `url` is the v1 singular that some rows still carry.
    """
    urls = entry.get("urls")
    if isinstance(urls, list):
        found = [url for url in urls if isinstance(url, str) and url]
        if found:
            return found
    url = entry.get("url")
    return [url] if isinstance(url, str) and url else []


def multipart_parts(entry: dict[str, Any]) -> list[dict[str, Any]]:
    parts = entry.get("parts")
    return parts if isinstance(parts, list) and parts else []


def fetch_catalogue(url: str) -> dict[str, Any]:
    def read_catalogue() -> bytes:
        request = urllib.request.Request(url, headers={"Accept-Encoding": "identity"})
        with urllib.request.urlopen(request, timeout=DOWNLOAD_TIMEOUT_SECS) as response:
            return response.read()

    raw = retry_network(read_catalogue, label=f"fetch toolchain catalogue {url}")
    try:
        payload = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise SystemExit(
            f"toolchain catalogue {url} is not valid UTF-8 JSON: {error}"
        ) from error
    if (
        not isinstance(payload, dict)
        or payload.get("schema_version") != CATALOGUE_SCHEMA_VERSION
    ):
        raise SystemExit(
            "toolchain catalogue is not a schema_version "
            f"{CATALOGUE_SCHEMA_VERSION} object"
        )
    return payload


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def download_verified(entry: dict[str, Any], output: Path) -> None:
    expected = str(entry["sha256"]).lower()
    temporary = output.with_suffix(output.suffix + ".part")
    urls = direct_urls(entry)

    def transfer() -> None:
        temporary.unlink(missing_ok=True)
        request = urllib.request.Request(
            urls[0], headers={"Accept-Encoding": "identity"}
        )
        with (
            urllib.request.urlopen(request, timeout=DOWNLOAD_TIMEOUT_SECS) as response,
            temporary.open("wb") as handle,
        ):
            shutil.copyfileobj(response, handle)

    try:
        if urls:
            retry_network(transfer, label=f"download {urls[0]}")
        else:
            write_multipart_asset(multipart_parts(entry), temporary)
        actual = sha256(temporary)
        if actual != expected:
            raise SystemExit(
                f"catalogued asset sha256 mismatch: expected {expected}, got {actual}"
            )
        temporary.replace(output)
    finally:
        temporary.unlink(missing_ok=True)


def safe_member(name: str) -> bool:
    normalized = name.replace("\\", "/")
    if normalized.startswith("/") or (len(normalized) >= 2 and normalized[1] == ":"):
        return False
    return ".." not in PurePosixPath(normalized).parts


def executable_name(tool: str, target: str) -> str:
    return f"{tool}.exe" if "-windows-" in target else tool


def extract_binary(archive: Path, *, tool: str, target: str, output_dir: Path) -> Path:
    expected = executable_name(tool, target)
    payload: bytes
    mode = 0o755

    if archive.name.endswith((".tar.gz", ".tgz")):
        with tarfile.open(archive, "r:gz") as handle:
            tar_members = handle.getmembers()
            if any(
                not safe_member(member.name) or member.issym() or member.islnk()
                for member in tar_members
            ):
                raise SystemExit(f"{archive.name} contains an unsafe path")
            tar_candidates = [
                member
                for member in tar_members
                if member.isfile() and PurePosixPath(member.name).name == expected
            ]
            if len(tar_candidates) != 1:
                raise SystemExit(
                    f"{archive.name} must contain exactly one {expected}; "
                    f"found {len(tar_candidates)}"
                )
            source = handle.extractfile(tar_candidates[0])
            if source is None:
                raise SystemExit(f"failed to read {expected} from {archive.name}")
            payload = source.read()
            mode = tar_candidates[0].mode
    elif archive.name.endswith(".zip"):
        with zipfile.ZipFile(archive) as handle:
            zip_members = handle.infolist()
            if any(not safe_member(member.filename) for member in zip_members):
                raise SystemExit(f"{archive.name} contains an unsafe path")
            zip_candidates = [
                member
                for member in zip_members
                if not member.is_dir()
                and PurePosixPath(member.filename.replace("\\", "/")).name == expected
            ]
            if len(zip_candidates) != 1:
                raise SystemExit(
                    f"{archive.name} must contain exactly one {expected}; "
                    f"found {len(zip_candidates)}"
                )
            payload = handle.read(zip_candidates[0])
    else:
        raise SystemExit(f"unsupported catalogue archive format: {archive.name}")

    output_dir.mkdir(parents=True, exist_ok=True)
    output = output_dir / expected
    temporary = output.with_suffix(output.suffix + ".part")
    temporary.write_bytes(payload)
    if os.name != "nt":
        temporary.chmod(mode | 0o111)
    temporary.replace(output)
    return output


# `link.exe` prints this banner before doing anything else, including before
# rejecting an option it does not understand.
MSVC_LINKER_BANNER = "Microsoft (R) Incremental Linker"


def _delegated_to_msvc_linker(tool: str, output: str) -> bool:
    """True when `dylint-link` reached MSVC's linker, whatever it exited with.

    `dylint-link` is a linker *wrapper*: it forwards its arguments to the
    platform linker. On Unix that is `cc`, which accepts `--version`. On MSVC
    it is `link.exe`, which does not -- it warns `LNK4044: unrecognized option
    '/-version'` and then fails `LNK1561: entry point must be defined`,
    because it was asked to link a program with no inputs.

    So requiring exit 0 here can never pass on Windows: the install aborts and
    leaves the tool uninstalled, which is why `dylint-link` could not be
    installed on an MSVC host at all. The banner is the real signal -- it says
    the wrapper resolved and handed off to a genuine linker, which is all this
    smoke can establish for a tool that has no version surface of its own.
    (The version string is already not checked for `dylint-link` below.)
    """
    return tool == "dylint-link" and MSVC_LINKER_BANNER in output


def smoke_version(binary: Path, *, tool: str, version: str, target: str) -> str:
    arguments = [str(binary), "--version"]
    if tool == "cargo-dylint":
        arguments = [str(binary), "dylint", "--version"]
    environment = os.environ.copy()
    if tool == "dylint-link":
        environment["RUSTUP_TOOLCHAIN"] = f"nightly-{target}"
    result = subprocess.run(
        arguments,
        check=False,
        capture_output=True,
        env=environment,
        text=True,
        timeout=SMOKE_TIMEOUT_SECS,
    )
    output = "\n".join(part.strip() for part in (result.stdout, result.stderr) if part)
    if _delegated_to_msvc_linker(tool, output):
        return output
    wrong_version = tool != "dylint-link" and version.removeprefix("v") not in output
    if result.returncode != 0 or wrong_version:
        raise SystemExit(
            f"{tool} smoke failed: exit={result.returncode}, output={output!r}"
        )
    return output


def install_tools(
    *,
    catalogue: dict[str, Any],
    tools: list[str],
    version: str,
    target: str,
    output_dir: Path,
) -> list[dict[str, str]]:
    installed: list[dict[str, str]] = []
    with tempfile.TemporaryDirectory(prefix="soldr-catalogued-tools-") as temp:
        root = Path(temp)
        stage = root / "bin"
        for tool in tools:
            entry = select_entry(catalogue, tool=tool, version=version, target=target)
            archive = root / str(entry["asset"])
            download_verified(entry, archive)
            binary = extract_binary(archive, tool=tool, target=target, output_dir=stage)
            reported = smoke_version(binary, tool=tool, version=version, target=target)
            installed.append(
                {
                    "tool": tool,
                    "asset": str(entry["asset"]),
                    "sha256": str(entry["sha256"]),
                    "version_output": reported,
                }
            )

        output_dir.mkdir(parents=True, exist_ok=True)
        for tool in tools:
            name = executable_name(tool, target)
            destination = output_dir / name
            temporary = destination.with_suffix(destination.suffix + ".part")
            shutil.copy2(stage / name, temporary)
            temporary.replace(destination)
    return installed


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tools", nargs="+")
    parser.add_argument("--target", required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--catalogue-url", default=DEFAULT_CATALOGUE_URL)
    args = parser.parse_args()

    result = install_tools(
        catalogue=fetch_catalogue(args.catalogue_url),
        tools=args.tools,
        version=args.version,
        target=args.target,
        output_dir=args.output_dir,
    )
    print(json.dumps({"target": args.target, "tools": result}, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
