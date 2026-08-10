"""Guard: the Linux Docker manifest-cache stage must cover every workspace member.

`Dockerfile.linux-build` has a `rust-manifests` stage that copies just the
manifests (plus the build scripts and proto inputs they read) so Docker can
cache the dependency build separately from the source tree. Cargo loads the
*whole* root workspace even when the build targets a single package, so a
member whose `Cargo.toml` is not copied fails the image with
"failed to load manifest for workspace member".

That is exactly how #772 was found: the stage still listed the four members
the workspace had before the probe crates landed. Enumerating the members by
hand is fine; letting the two lists drift silently is not. This checker
compares them, so adding a workspace member without touching the Dockerfile
fails lint instead of failing the image.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DOCKERFILE = ROOT / "Dockerfile.linux-build"
WORKSPACE_MANIFEST = ROOT / "Cargo.toml"
MANIFEST_STAGE = "rust-manifests"


def workspace_members(manifest_text: str) -> list[str]:
    """Return the `[workspace] members = [...]` entries, in declaration order."""
    match = re.search(
        r"^\[workspace\]\s*$(.*?)(?=^\[|\Z)",
        manifest_text,
        re.MULTILINE | re.DOTALL,
    )
    if match is None:
        raise SystemExit("docker-manifest-guard: no [workspace] table in Cargo.toml")
    members = re.search(r"members\s*=\s*\[(.*?)\]", match.group(1), re.DOTALL)
    if members is None:
        raise SystemExit("docker-manifest-guard: no `members` key in [workspace]")
    return re.findall(r'"([^"]+)"', members.group(1))


def stage_body(dockerfile_text: str, stage: str) -> str:
    """Return the lines belonging to `stage`, up to the next FROM."""
    lines = dockerfile_text.splitlines()
    start: int | None = None
    for index, line in enumerate(lines):
        if re.match(rf"^FROM\s+\S+\s+AS\s+{re.escape(stage)}\s*$", line):
            start = index + 1
            break
    if start is None:
        raise SystemExit(f"docker-manifest-guard: no `AS {stage}` stage in {DOCKERFILE.name}")
    for index in range(start, len(lines)):
        if lines[index].startswith("FROM "):
            return "\n".join(lines[start:index])
    return "\n".join(lines[start:])


def copied_paths(stage_text: str) -> set[str]:
    """Return every source path named by a COPY in the stage.

    Line continuations are joined first so a multi-line COPY is read whole.
    The final argument of a COPY is the destination and is dropped; flags
    (`--from=`, `--chmod=`) are dropped too.
    """
    joined = re.sub(r"\\\s*\n\s*", " ", stage_text)
    paths: set[str] = set()
    for line in joined.splitlines():
        line = line.strip()
        if not line.upper().startswith("COPY "):
            continue
        args = [arg for arg in line.split()[1:] if not arg.startswith("--")]
        for source in args[:-1]:
            paths.add(source.strip('"'))
    return paths


def is_covered(required: str, copied: set[str]) -> bool:
    """True when `required` is copied outright or sits under a copied directory."""
    if required in copied:
        return True
    return any(required.startswith(f"{candidate}/") for candidate in copied)


def missing_inputs(members: list[str], copied: set[str]) -> list[str]:
    """Return the member inputs the manifest stage fails to copy."""
    missing: list[str] = []
    for member in members:
        required = [f"{member}/Cargo.toml"]
        if (ROOT / member / "build.rs").is_file():
            required.append(f"{member}/build.rs")
        if (ROOT / member / "proto").is_dir():
            required.append(f"{member}/proto")
        missing.extend(path for path in required if not is_covered(path, copied))
    return missing


def main() -> int:
    members = workspace_members(WORKSPACE_MANIFEST.read_text(encoding="utf-8"))
    copied = copied_paths(stage_body(DOCKERFILE.read_text(encoding="utf-8"), MANIFEST_STAGE))
    missing = missing_inputs(members, copied)
    if missing:
        print(
            f"docker-manifest-guard: {DOCKERFILE.name} stage `{MANIFEST_STAGE}` does not "
            "copy these workspace inputs:",
            file=sys.stderr,
        )
        for path in missing:
            print(f"  - {path}", file=sys.stderr)
        print(
            "Add a COPY for each (and a source stub below it) so the cache stage "
            "can load the whole workspace.",
            file=sys.stderr,
        )
        return 1
    print(f"docker-manifest-guard: OK ({len(members)} workspace members covered)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
