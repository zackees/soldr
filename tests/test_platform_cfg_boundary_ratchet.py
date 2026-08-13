"""Ratchets for the #2493 platform-cfg directory boundary."""

import importlib.util
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
MODULE_PATH = REPO_ROOT / ".github" / "scripts" / "platform_cfg_boundary_ratchet.py"
ALLOWLIST = (
    REPO_ROOT
    / "dylints"
    / "ban_platform_cfg_outside_boundary"
    / "src"
    / "allowlist.txt"
)

_spec = importlib.util.spec_from_file_location("boundary_ratchet", MODULE_PATH)
assert _spec is not None
assert _spec.loader is not None
_ratchet = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_ratchet)


def test_allowlist_exactly_matches_current_boundary_violations() -> None:
    actual = _ratchet.violations()
    allowlisted = _ratchet.read_allowlist(ALLOWLIST)
    assert actual == allowlisted, (
        "boundary allowlist must equal the measured violations; run "
        "`python .github/scripts/platform_cfg_boundary_ratchet.py` for the diff"
    )
    assert not _ratchet.verify(ALLOWLIST)


def test_allowlist_contains_only_shrinking_entries() -> None:
    allowlisted = _ratchet.read_allowlist(ALLOWLIST)
    for entry in allowlisted:
        assert entry.startswith("crates/"), entry


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
