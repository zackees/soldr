#!/usr/bin/env python3
"""Write the release archive's `manifest.json` (soldr#2469 step 2.2).

The manifest is the release describing itself — versions, target triples,
per-binary sha256s, provenance — and setup-soldr, the npm install wrapper and
ad-hoc tooling all read it. It was assembled by 127 lines of inline bash that
built JSON with a heredoc, which is how published v0.8.29 shipped
`crgx.source_commit` holding a two-line value with `cargo_chef.source_commit`
reading `"unknown"`: a `$GITHUB_ENV` writer had terminated lines with a
literal backslash-n and the heredoc happily interpolated it.

`json.dumps` makes that class of corruption structurally impossible — a
newline inside a value is escaped rather than able to end a field — which is
the main reason this belongs in Python rather than in a heredoc.

Deliberately preserved from the bash, byte for byte:

* a macOS `.dSYM` is a directory, hashed as the `tar -cf -` stream of its
  contents. `tar` is invoked as a subprocess rather than reimplemented with
  `tarfile`, because the digest is *published*: member order, mtimes and
  uid/gid make the two implementations disagree, and `verify_release_manifest`
  explicitly cannot re-derive this value to catch a drift.
* Windows `.pdb` is REQUIRED and its absence fails the build; Linux `.dwp` and
  macOS `.dSYM` are OPTIONAL, because the default release profile does not
  emit them and a missing sidecar is silent.
* format tags must match `release_sidecar.rs::DebugSidecarFormat::as_manifest_str`.

Usage (CI):
    python3 .github/scripts/release_manifest.py \
        --version v0.9.2 --commit-sha <sha> --target x86_64-unknown-linux-gnu \
        --package-dir dist/package
"""

from __future__ import annotations

import argparse
import datetime
import hashlib
import json
import re
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]

SCHEMA_VERSION = 3
ARCHIVE_FORMAT = "tar.zst"
ARCHIVE_COMPRESSION_LEVEL = 19

# (source file, regex, human name) for each pinned version the manifest
# reports. Read from the tree rather than passed in, so the manifest cannot
# disagree with the source it was built from.
ZCCACHE_VERSION = ("_vender/zccache/Cargo.toml", r'^version = "(.*)"', "zccache")
CRGX_VERSION = (
    "crates/soldr-fetch/src/fetch/mod.rs",
    r'MANAGED_CRGX_VERSION: &str = "(.*)";',
    "MANAGED_CRGX_VERSION",
)
CARGO_CHEF_VERSION = (
    "crates/soldr-fetch/src/fetch/known_tools.rs",
    r'CARGO_CHEF_PINNED_VERSION: &str = "(.*)";',
    "CARGO_CHEF_PINNED_VERSION",
)


class ManifestError(RuntimeError):
    """A precondition the manifest cannot be written without."""


def read_pinned_version(root: Path, spec: tuple[str, str, str]) -> str:
    """First capture of `spec`'s regex in its file.

    Empty is an error rather than a default: a manifest that reports `""` for
    a pinned version looks answered, which the v0.8.29 provenance incident
    showed is worse than absent.
    """
    relative, pattern, label = spec
    text = (root / relative).read_text(encoding="utf-8")
    match = re.search(pattern, text, re.MULTILINE)
    if not match or not match.group(1):
        raise ManifestError(f"could not read {label} from {relative}")
    return match.group(1)


def binary_suffix(target: str) -> str:
    return ".exe" if target.endswith("-pc-windows-msvc") else ""


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def sha256_dsym(path: Path) -> str:
    """Hash a `.dSYM` bundle as the `tar -cf -` stream of its contents.

    Shells out on purpose — see the module docstring. This digest is
    published, and `tarfile` would produce different bytes than the `tar` the
    bash used, silently invalidating it for anyone verifying by hand.
    """
    completed = subprocess.run(
        ["tar", "-cf", "-", "-C", str(path.parent), path.name],
        stdout=subprocess.PIPE,
        check=True,
    )
    return hashlib.sha256(completed.stdout).hexdigest()


def find_sidecar(
    package_dir: Path, names: list[str], directory: bool = False
) -> Path | None:
    for name in names:
        candidate = package_dir / name
        if candidate.is_dir() if directory else candidate.is_file():
            return candidate
    return None


def collect_debug_info(package_dir: Path, suffix: str) -> list[dict[str, str]]:
    """Debug sidecars staged beside the binary (soldr#786)."""
    entries: list[dict[str, str]] = []
    if suffix:
        pdb = find_sidecar(package_dir, ["soldr.pdb", "soldr_cli.pdb"])
        if pdb is None:
            listing = "\n".join(sorted(p.name for p in package_dir.iterdir()))
            raise ManifestError(
                "missing soldr PDB sidecar in "
                f"{package_dir} for Windows release; contents:\n{listing}"
            )
        entries.append({"name": pdb.name, "sha256": sha256_file(pdb), "format": "pdb"})
    dwp = find_sidecar(package_dir, ["soldr.dwp", "soldr_cli.dwp"])
    if dwp is not None:
        entries.append({"name": dwp.name, "sha256": sha256_file(dwp), "format": "dwp"})
    dsym = find_sidecar(package_dir, ["soldr.dSYM", "soldr_cli.dSYM"], directory=True)
    if dsym is not None:
        entries.append(
            {"name": dsym.name, "sha256": sha256_dsym(dsym), "format": "dsym"}
        )
    return entries


def build_manifest(
    *,
    version: str,
    commit_sha: str,
    target: str,
    suffix: str,
    digests: dict[str, str],
    debug_info: list[dict[str, str]],
    versions: dict[str, str],
    commits: dict[str, str],
    built_at: str,
) -> dict:
    """Pure assembly, so the shape is unit-testable without a staged tree."""
    return {
        "schema_version": SCHEMA_VERSION,
        "soldr": {
            "version": version,
            "target": target,
            "binary": f"soldr{suffix}",
            "sha256": digests["soldr"],
            "sidecars": [
                {"name": f"soldr-daemon{suffix}", "sha256": digests["soldr-daemon"]}
            ],
            "debug_info": debug_info,
            "commit_sha": commit_sha,
        },
        "zccache": {
            "version": versions["zccache"],
            "target": target,
            "embedded": True,
        },
        "crgx": {
            "version": versions["crgx"],
            "target": target,
            "binary": f"crgx{suffix}",
            "sha256": digests["crgx"],
            "source_commit": commits["crgx"],
        },
        "cargo_chef": {
            "version": versions["cargo_chef"],
            "target": target,
            "binary": f"cargo-chef{suffix}",
            "sha256": digests["cargo-chef"],
            "source_commit": commits["cargo_chef"],
        },
        "archive": {
            "format": ARCHIVE_FORMAT,
            "compression_level": ARCHIVE_COMPRESSION_LEVEL,
        },
        "built_at": built_at,
    }


def utc_now() -> str:
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True)
    parser.add_argument("--commit-sha", required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--package-dir", type=Path, default=Path("dist/package"))
    parser.add_argument("--repo-root", type=Path, default=REPO_ROOT)
    parser.add_argument("--crgx-source-commit", default="unknown")
    parser.add_argument("--cargo-chef-source-commit", default="unknown")
    args = parser.parse_args(argv)

    suffix = binary_suffix(args.target)
    package_dir: Path = args.package_dir
    try:
        digests = {
            name: sha256_file(package_dir / f"{name}{suffix}")
            for name in ("soldr", "soldr-daemon", "crgx", "cargo-chef")
        }
        manifest = build_manifest(
            version=args.version,
            commit_sha=args.commit_sha,
            target=args.target,
            suffix=suffix,
            digests=digests,
            debug_info=collect_debug_info(package_dir, suffix),
            versions={
                "zccache": read_pinned_version(args.repo_root, ZCCACHE_VERSION),
                "crgx": read_pinned_version(args.repo_root, CRGX_VERSION),
                "cargo_chef": read_pinned_version(args.repo_root, CARGO_CHEF_VERSION),
            },
            commits={
                "crgx": args.crgx_source_commit or "unknown",
                "cargo_chef": args.cargo_chef_source_commit or "unknown",
            },
            built_at=utc_now(),
        )
    except (ManifestError, OSError) as error:
        print(str(error), file=sys.stderr)
        return 1

    rendered = json.dumps(manifest, indent=2) + "\n"
    (package_dir / "manifest.json").write_text(rendered, encoding="utf-8")
    print("--- manifest.json ---")
    print(rendered, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
