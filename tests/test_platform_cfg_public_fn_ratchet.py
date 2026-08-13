from pathlib import Path

from conftest import load_script_module

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / ".github" / "scripts" / "platform_cfg_public_fn_ratchet.py"
ALLOWLIST = ROOT / "dylints" / "ban_platform_cfg_in_public_fn" / "src" / "allowlist.txt"
_ratchet = load_script_module(SCRIPT, "platform_cfg_public_fn_ratchet")


def test_allowlist_exactly_matches_current_production_debt() -> None:
    actual = _ratchet.violations(ROOT / "crates" / "soldr-cli" / "src")
    allowed = _ratchet.read_allowlist(ALLOWLIST)
    assert actual == allowed
    assert len(actual) == 23


def test_detector_ignores_private_functions_comments_and_strings(
    tmp_path: Path,
) -> None:
    source_root = tmp_path / "crates" / "soldr-cli" / "src"
    source_root.mkdir(parents=True)
    (source_root / "sample.rs").write_text(
        """
fn private() { if cfg!(windows) {} }
pub fn neutral() { let _ = "#[cfg(unix)]"; /* cfg!(target_os = "linux") */ }
pub fn raw_string_is_neutral() { let _ = r###"cfg!(target_arch = "x86_64")"###; }
pub(crate) fn violation() { if cfg!(target_arch = "x86_64") {} }
pub extern "C" fn extern_violation() { if cfg!(windows) {} }
pub unsafe extern "system" fn unsafe_extern_violation() { if cfg!(unix) {} }
""",
        encoding="utf-8",
    )
    assert _ratchet.violations(source_root) == {
        "crates/soldr-cli/src/sample.rs::extern_violation",
        "crates/soldr-cli/src/sample.rs::unsafe_extern_violation",
        "crates/soldr-cli/src/sample.rs::violation",
    }


def test_detector_includes_outer_attributes_and_distinguishes_duplicates(
    tmp_path: Path,
) -> None:
    source_root = tmp_path / "crates" / "soldr-cli" / "src"
    source_root.mkdir(parents=True)
    (source_root / "sample.rs").write_text(
        """
#[cfg(windows)]
pub fn selected() {}
#[cfg(unix)]
pub fn selected() {}
#[cfg_attr(target_os = "linux", allow(dead_code))]
pub(crate) fn attributed() {}
#[cfg_attr(all(feature = "x"), cfg(windows))]
pub(crate) fn nested_attribute() {}
#[cfg_attr(all(feature = "x"), cfg(target_abi = "eabihf"))]
pub(crate) fn nested_abi_attribute() {}
pub(\tself) fn private_to_module() { if cfg!(windows) {} }
pub(in\n self) fn also_private_to_module() { if cfg!(unix) {} }
#[cfg(test)]
pub fn test_fixture() { if cfg!(windows) {} }
mod a {
    pub fn same() { if cfg!(windows) {} }
}
mod b {
    pub fn same() { if cfg!(windows) {} }
}
""",
        encoding="utf-8",
    )
    assert _ratchet.violations(source_root) == {
        "crates/soldr-cli/src/sample.rs::a::same",
        'crates/soldr-cli/src/sample.rs::attributed@cfg_attr(target_os="linux",allow(dead_code))',
        "crates/soldr-cli/src/sample.rs::b::same",
        'crates/soldr-cli/src/sample.rs::nested_abi_attribute@cfg_attr(all(feature="x"),cfg(target_abi="eabihf"))',
        'crates/soldr-cli/src/sample.rs::nested_attribute@cfg_attr(all(feature="x"),cfg(windows))',
        "crates/soldr-cli/src/sample.rs::selected@cfg(unix)",
        "crates/soldr-cli/src/sample.rs::selected@cfg(windows)",
    }


def test_inner_cfg_test_does_not_hide_production_violation(tmp_path: Path) -> None:
    source_root = tmp_path / "crates" / "soldr-cli" / "src"
    source_root.mkdir(parents=True)
    (source_root / "sample.rs").write_text(
        """
pub fn production() {
    #[cfg(test)]
    let fixture = 1;
    if cfg!(target_abi = "eabihf") { let _ = fixture; }
}
""",
        encoding="utf-8",
    )
    assert _ratchet.violations(source_root) == {
        "crates/soldr-cli/src/sample.rs::production"
    }


def test_module_qualified_keys_do_not_transfer_between_modules(tmp_path: Path) -> None:
    source_root = tmp_path / "crates" / "soldr-cli" / "src"
    source_root.mkdir(parents=True)
    sample = source_root / "sample.rs"
    sample.write_text(
        "mod a {\n    pub fn same() { if cfg!(windows) {} }\n}\n", encoding="utf-8"
    )
    allowlist = tmp_path / "allowlist.txt"
    allowlist.write_text("crates/soldr-cli/src/sample.rs::a::same\n", encoding="utf-8")
    assert _ratchet.verify(source_root, allowlist) == []
    sample.write_text(
        "mod c {\n    pub fn same() { if cfg!(windows) {} }\n}\n", encoding="utf-8"
    )
    assert _ratchet.verify(source_root, allowlist) == [
        "new violation: crates/soldr-cli/src/sample.rs::c::same",
        "stale allowlist entry: crates/soldr-cli/src/sample.rs::a::same",
    ]


def test_verify_rejects_new_and_stale_entries(tmp_path: Path) -> None:
    source_root = tmp_path / "crates" / "soldr-cli" / "src"
    source_root.mkdir(parents=True)
    (source_root / "sample.rs").write_text(
        "pub fn violation() { if cfg!(windows) {} }\n", encoding="utf-8"
    )
    allowlist = tmp_path / "allowlist.txt"
    allowlist.write_text("crates/soldr-cli/src/sample.rs::stale\n", encoding="utf-8")
    assert _ratchet.verify(source_root, allowlist) == [
        "new violation: crates/soldr-cli/src/sample.rs::violation",
        "stale allowlist entry: crates/soldr-cli/src/sample.rs::stale",
    ]


def test_dylint_crate_and_ci_wiring_exist() -> None:
    lint = ROOT / "dylints" / "ban_platform_cfg_in_public_fn"
    assert (lint / "src" / "lib.rs").is_file()
    assert (lint / "ui" / "disallowed_visibility.rs").is_file()
    workflow = (ROOT / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")
    assert "Build platform-neutral function boundary lint" in workflow
    assert "Test platform-neutral function boundary lint" in workflow
