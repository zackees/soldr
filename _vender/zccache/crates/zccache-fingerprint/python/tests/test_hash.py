"""Tests for the Api hashing functions."""

import os
import tempfile

from zccache.fingerprint import Api


def _make_tree(tmp: str, files: dict[str, str]) -> None:
    for rel, content in files.items():
        path = os.path.join(tmp, rel)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w") as f:
            f.write(content)


def test_hash_deterministic() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(tmp, {"a.rs": "fn main() {}", "b.rs": "fn foo() {}"})
        h1 = Api.hash_files(tmp, ["rs"])
        h2 = Api.hash_files(tmp, ["rs"])
        assert h1 == h2
        assert len(h1) == 64  # blake3 hex digest


def test_hash_changes_on_edit() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(tmp, {"a.rs": "v1"})
        h1 = Api.hash_files(tmp, ["rs"])

        _make_tree(tmp, {"a.rs": "v2"})
        h2 = Api.hash_files(tmp, ["rs"])
        assert h1 != h2


def test_hash_changes_on_add() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(tmp, {"a.rs": "a"})
        h1 = Api.hash_files(tmp, ["rs"])

        _make_tree(tmp, {"a.rs": "a", "b.rs": "b"})
        h2 = Api.hash_files(tmp, ["rs"])
        assert h1 != h2


def test_hash_changes_on_remove() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(tmp, {"a.rs": "a", "b.rs": "b"})
        h1 = Api.hash_files(tmp, ["rs"])

        os.remove(os.path.join(tmp, "b.rs"))
        h2 = Api.hash_files(tmp, ["rs"])
        assert h1 != h2


def test_hash_ignores_excluded_dirs() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(tmp, {"src/a.rs": "a", ".git/hook.rs": "hook"})
        h_no_excl = Api.hash_files(tmp, ["rs"])
        h_excl = Api.hash_files(tmp, ["rs"], [".git"])
        assert h_no_excl != h_excl  # .git/hook.rs is excluded


def test_hash_files_glob() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(tmp, {"src/a.rs": "r", "src/b.py": "p", "lib/c.rs": "r"})
        h = Api.hash_files_glob(tmp, ["src/**/*.rs"])
        assert len(h) == 64

        # Different include pattern -> different files -> different hash.
        h2 = Api.hash_files_glob(tmp, ["**/*.rs"])
        assert h != h2  # lib/c.rs is now included


def test_hash_directory_convenience() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(tmp, {"a.h": "h", "b.cpp": "c", "c.txt": "t"})
        h = Api.hash_directory(tmp, "**/*.h,**/*.cpp")
        assert len(h) == 64


def test_walk_and_hash() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(tmp, {"b.rs": "beta", "a.rs": "alpha"})
        result = Api.walk_and_hash(tmp, ["rs"])
        assert len(result) == 2
        # Sorted by relative path.
        assert result[0][0] == "a.rs"
        assert result[1][0] == "b.rs"
        # Each entry is (rel_path, blake3_hex).
        assert len(result[0][1]) == 64
