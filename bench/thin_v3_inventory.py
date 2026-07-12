#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = []
# ///
"""Inventory cache layers and quantify cross-layer byte duplication.

Examples:
    uv run --no-project python bench/thin_v3_inventory.py \
      --layer thin-v1=/tmp/thin-v1 --layer zccache=/tmp/zccache \
      --layer cook-base=/tmp/cook/base --output results.json

The compressed size is a deterministic gzip stream over path, mode, size, and
content. It is intended for repeatable comparisons; provider/container framing
overhead is reported separately by the real upload benchmark.
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import json
import os
import stat
import time
from collections import Counter, defaultdict
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import BinaryIO, Iterable

CHUNK = 1024 * 1024


@dataclass(frozen=True)
class FileRecord:
    layer: str
    path: str
    size: int
    sha256: str
    artifact_class: str


def classify(path: str) -> str:
    normalized = path.replace("\\", "/")
    name = normalized.rsplit("/", 1)[-1]
    suffix = Path(name).suffix.lower()
    if "/incremental/" in f"/{normalized}/":
        return "incremental"
    if "/.fingerprint/" in f"/{normalized}/":
        return "cargo_fingerprint"
    if "/build/" in f"/{normalized}/" and "/out/" in f"/{normalized}/":
        return "build_script_out_dir"
    if name in {"output", "root-output", "invoked.timestamp"}:
        return "build_script_metadata"
    if name.startswith("build-script-build"):
        return "build_script_executable"
    if suffix == ".rlib":
        return "rlib"
    if suffix == ".rmeta":
        return "rmeta"
    if suffix == ".d":
        return "dep_info"
    if suffix in {".so", ".dylib", ".dll"}:
        return "shared_or_proc_macro"
    if suffix in {".a", ".lib"}:
        return "native_static_library"
    if suffix in {".o", ".obj"}:
        return "native_object"
    if suffix in {".pdb", ".dwo"} or ".dsym/" in normalized.lower():
        return "split_debug"
    if name in {"index.bin", ".global-cache"} or suffix in {".db", ".sqlite"}:
        return "database_or_index"
    return "other"


def iter_files(root: Path) -> Iterable[Path]:
    if not root.exists():
        return
    for directory, dirs, files in os.walk(root):
        dirs.sort()
        files.sort()
        base = Path(directory)
        for name in files:
            path = base / name
            try:
                mode = path.lstat().st_mode
            except OSError:
                continue
            if stat.S_ISREG(mode):
                yield path


def hash_and_compress(path: Path, compressed: BinaryIO) -> tuple[int, str]:
    digest = hashlib.sha256()
    size = 0
    with path.open("rb") as source:
        while chunk := source.read(CHUNK):
            size += len(chunk)
            digest.update(chunk)
            compressed.write(chunk)
    return size, digest.hexdigest()


def scan_layer(name: str, root: Path) -> tuple[list[FileRecord], int, float]:
    started = time.perf_counter()
    records: list[FileRecord] = []
    counter = _CountingSink()
    with gzip.GzipFile(fileobj=counter, mode="wb", compresslevel=6, mtime=0) as archive:
        for path in iter_files(root):
            relative = path.relative_to(root).as_posix()
            metadata = path.stat()
            header = f"{relative}\0{metadata.st_mode & 0o777:o}\0{metadata.st_size}\0".encode()
            archive.write(header)
            size, digest = hash_and_compress(path, archive)
            records.append(FileRecord(name, relative, size, digest, classify(relative)))
    return records, counter.count, time.perf_counter() - started


class _CountingSink:
    def __init__(self) -> None:
        self.count = 0

    def write(self, data: bytes) -> int:
        self.count += len(data)
        return len(data)

    def flush(self) -> None:
        pass


def build_report(layers: list[tuple[str, Path]]) -> dict[str, object]:
    all_records: list[FileRecord] = []
    layer_reports: dict[str, object] = {}
    for name, root in layers:
        records, compressed_bytes, scan_seconds = scan_layer(name, root)
        all_records.extend(records)
        class_counts = Counter(record.artifact_class for record in records)
        class_bytes = Counter()
        for record in records:
            class_bytes[record.artifact_class] += record.size
        raw_bytes = sum(record.size for record in records)
        layer_reports[name] = {
            "root": str(root),
            "file_count": len(records),
            "raw_bytes": raw_bytes,
            "compressed_bytes": compressed_bytes,
            "compression_ratio": compressed_bytes / raw_bytes if raw_bytes else 0.0,
            "scan_seconds": scan_seconds,
            "classes": {
                key: {"file_count": class_counts[key], "raw_bytes": class_bytes[key]}
                for key in sorted(class_counts)
            },
        }

    by_hash: dict[tuple[str, int], list[FileRecord]] = defaultdict(list)
    for record in all_records:
        by_hash[(record.sha256, record.size)].append(record)
    duplicate_groups = []
    duplicate_bytes = 0
    duplicate_files = 0
    for (digest, size), records in by_hash.items():
        owners = sorted({record.layer for record in records})
        if len(owners) < 2 or size == 0:
            continue
        copies = len(records)
        wasted = size * (copies - 1)
        duplicate_bytes += wasted
        duplicate_files += copies - 1
        duplicate_groups.append(
            {
                "sha256": digest,
                "size": size,
                "copies": copies,
                "duplicate_bytes": wasted,
                "layers": owners,
                "paths": [asdict(record) for record in records],
            }
        )
    duplicate_groups.sort(key=lambda row: (-int(row["duplicate_bytes"]), str(row["sha256"])))
    total_raw = sum(record.size for record in all_records)
    unique_raw = sum(size for (_, size) in by_hash)
    return {
        "schema_version": 1,
        "generated_at_unix_seconds": int(time.time()),
        "layers": layer_reports,
        "combined": {
            "file_count": len(all_records),
            "raw_bytes": total_raw,
            "unique_bytes": unique_raw,
            "duplicate_bytes": duplicate_bytes,
            "duplicate_file_count": duplicate_files,
            "duplicate_ratio": duplicate_bytes / total_raw if total_raw else 0.0,
        },
        "duplicate_groups": duplicate_groups,
    }


def parse_layer(value: str) -> tuple[str, Path]:
    name, separator, path = value.partition("=")
    if not separator or not name or not path:
        raise argparse.ArgumentTypeError("layer must be NAME=PATH")
    return name, Path(path)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--layer", action="append", type=parse_layer, required=True)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--include-files", action="store_true")
    parser.add_argument(
        "--require-nonempty",
        action="append",
        default=[],
        metavar="LAYER",
        help="Fail when a named layer has no files (repeatable).",
    )
    args = parser.parse_args()
    report = build_report(args.layer)
    unknown = sorted(set(args.require_nonempty) - set(report["layers"]))
    if unknown:
        parser.error(f"--require-nonempty names unknown layers: {', '.join(unknown)}")
    empty = [name for name in args.require_nonempty if report["layers"][name]["file_count"] == 0]
    if empty:
        parser.error(f"required layers are empty: {', '.join(empty)}")
    if args.include_files:
        report["files"] = [
            asdict(record) for name, root in args.layer for record in scan_layer(name, root)[0]
        ]
    rendered = json.dumps(report, indent=2, sort_keys=True) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(rendered, encoding="utf-8")
    print(rendered, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
