"""Ratchets for the #2493 platform-cfg directory boundary."""

import importlib.util
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
MODULE_PATH = REPO_ROOT / ".github" / "scripts" / "platform_cfg_boundary_ratchet.py"
_spec = importlib.util.spec_from_file_location("boundary_ratchet", MODULE_PATH)
assert _spec is not None
assert _spec.loader is not None
_ratchet = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_ratchet)


def test_workspace_has_zero_boundary_violations() -> None:
    assert _ratchet.violations() == set()


def test_test_examples_and_benches_are_scanned(tmp_path: Path) -> None:
    roots = ["tests", "examples", "benches"]
    original_root = getattr(_ratchet, "SOURCE_ROOT")
    test_root = tmp_path / "crates"
    try:
        setattr(_ratchet, "SOURCE_ROOT", test_root)
        for root in roots:
            path = test_root / "demo" / root / "host.rs"
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("#[cfg(windows)] fn host_only() {}", encoding="utf-8")
        assert len(_ratchet.violations()) == 3
    finally:
        setattr(_ratchet, "SOURCE_ROOT", original_root)


def test_detector_flags_private_cfg_and_statements() -> None:
    masked = _ratchet.mask_comments_and_strings(
        """
        #[cfg(target_os = "windows")]
        fn private() { if cfg!(unix) {} }
        pub fn outer() {
            #[cfg_attr(windows, allow(dead_code))]
            let _ = 1;
        }
        """
    )
    invocations = _ratchet.platform_cfg_invocations(masked)
    assert len(invocations) == 3, invocations


def test_detector_flags_crate_level_host_cfg() -> None:
    masked = _ratchet.mask_comments_and_strings("#![cfg(unix)]\nfn main() {}")
    assert _ratchet.platform_cfg_invocations(masked) == ["#![cfg(unix)"]


def test_detector_ignores_comments_strings_and_feature_cfgs() -> None:
    masked = _ratchet.mask_comments_and_strings(
        '''
        fn neutral() {
            let _ = "#[cfg(windows)]";
            let _ = r###"cfg!(target_os = "linux")"###;
            /* cfg!(unix) */
            #[cfg(feature = "tokio-console")]
            let _ = 1;
        }
        '''
    )
    assert _ratchet.platform_cfg_invocations(masked) == []


def test_detector_finds_concrete_tree_references() -> None:
    masked = _ratchet.mask_comments_and_strings(
        "crate::platform_imp::fs::x(); platform_win::y();"
    )
    assert set(_ratchet.concrete_tree_references(masked)) == {
        "platform_imp",
        "platform_win",
    }


def test_detector_finds_native_platform_references() -> None:
    masked = _ratchet.mask_comments_and_strings(
        "use std::os::unix::fs::PermissionsExt; windows_sys::Win32::Foundation::HANDLE;"
    )
    assert set(_ratchet.native_platform_references(masked)) == {
        "std::os::unix",
        "windows_sys",
    }
    masked = _ratchet.mask_comments_and_strings("platform_win32; platform_windows;")
    assert _ratchet.concrete_tree_references(masked) == []


def test_boundary_files_are_never_violations() -> None:
    assert _ratchet.is_boundary(
        Path("crates/soldr-platform/src/lib.rs")
    )
    assert _ratchet.is_boundary(
        Path("crates/soldr-platform/src/platform_win/fs/identity.rs")
    )
    assert not _ratchet.is_boundary(
        Path("crates/soldr-platform/src/platform/fs/identity.rs")
    )
    assert not _ratchet.is_boundary(Path("crates/soldr-cli/src/broker_spawn.rs"))
