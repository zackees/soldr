from __future__ import annotations

import base64
import csv
import hashlib
import io
import zipfile
from collections.abc import Iterable
from pathlib import Path


def record_entry(wheel: Path) -> str:
    with zipfile.ZipFile(wheel) as zf:
        records = [
            name
            for name in zf.namelist()
            if name.endswith(".dist-info/RECORD") and "/" in name
        ]
    if len(records) != 1:
        raise RuntimeError(
            f"expected exactly one wheel RECORD in {wheel}, found {records}"
        )
    return records[0]


def record_hash(payload: bytes) -> str:
    digest = base64.urlsafe_b64encode(hashlib.sha256(payload).digest())
    return "sha256=" + digest.rstrip(b"=").decode("ascii")


def render_record(rows: Iterable[tuple[str, bytes]], record_name: str) -> bytes:
    buffer = io.StringIO(newline="")
    writer = csv.writer(buffer, lineterminator="\n")
    for name, payload in rows:
        writer.writerow([name, record_hash(payload), str(len(payload))])
    writer.writerow([record_name, "", ""])
    return buffer.getvalue().encode("utf-8")


def validate_record(wheel: Path) -> None:
    errors: list[str] = []
    with zipfile.ZipFile(wheel) as zf:
        names = zf.namelist()
        if len(names) != len(set(names)):
            errors.append("wheel contains duplicate archive entries")
        record_name = record_entry(wheel)
        record_rows: dict[str, tuple[str, str]] = {}
        record_text = zf.read(record_name).decode("utf-8")
        for index, row in enumerate(csv.reader(io.StringIO(record_text)), start=1):
            if len(row) != 3:
                errors.append(
                    f"{record_name}:{index} expected 3 columns, found {len(row)}"
                )
                continue
            name, hash_value, size = row
            if name in record_rows:
                errors.append(f"{record_name}:{index} duplicates entry {name}")
            record_rows[name] = (hash_value, size)

        missing = sorted(set(names) - set(record_rows))
        extra = sorted(set(record_rows) - set(names))
        if missing:
            errors.append("RECORD is missing entries: " + ", ".join(missing))
        if extra:
            errors.append("RECORD contains non-archive entries: " + ", ".join(extra))

        for name in names:
            recorded = record_rows.get(name)
            if recorded is None:
                continue
            hash_value, size = recorded
            if name == record_name:
                if hash_value or size:
                    errors.append(f"{record_name} must have empty hash and size")
                continue
            payload = zf.read(name)
            expected_hash = record_hash(payload)
            expected_size = str(len(payload))
            if hash_value != expected_hash:
                errors.append(f"{name} hash mismatch")
            if size != expected_size:
                errors.append(f"{name} size mismatch: {size} != {expected_size}")

    if errors:
        raise RuntimeError(
            "wheel RECORD does not match file contents:\n- " + "\n- ".join(errors)
        )
