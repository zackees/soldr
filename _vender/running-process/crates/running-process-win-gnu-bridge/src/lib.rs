//! GNU-Windows bridge for the MSVC-obligatory Windows API surface (#580).
//!
//! Windows builds of this workspace are effectively pinned to
//! `x86_64-pc-windows-msvc`. This crate is the **build seam** that lets
//! `x86_64-pc-windows-gnu` builds reach the same Windows API surface the
//! rest of the workspace depends on, without regressing the MSVC path.
//!
//! The guiding principle from the issue is *direct where possible, bridge
//! only where necessary*. The first proof point is the **ConPTY** surface
//! (`CreatePseudoConsole` / `ResizePseudoConsole` / `ClosePseudoConsole`),
//! which `crates/running-process` uses directly (#150).
//!
//! ## Mechanism per MSVC-obligatory API surface
//!
//! | Surface | Mechanism | Notes |
//! | --- | --- | --- |
//! | ConPTY (`CreatePseudoConsole` etc.) | **direct** | `windows-sys` bundles a per-target import library (`windows-targets` → `windows_x86_64_gnu`), so the GNU linker resolves these symbols with no Windows SDK and no MSVC `link.exe`. See [`conpty`]. |
//! | `retour` inline detours / DLL injection | **out-of-scope** | ABI/`iced-x86` risk; tracked as a follow-up. |
//! | `libsqlite3-sys` (bundled) | **out-of-scope** | Needs a C compiler under GNU (`gcc.exe`); tracked as a follow-up. |
//! | `procdump` / DbgHelp minidump | **out-of-scope** | Dev-only (`test-watchdog`); not on the shipped path. |
//!
//! No **bridge** (import-lib shim / C shim) mechanism is required for the
//! in-scope ConPTY surface — `windows-sys` already links directly under
//! GNU. The bridge column is kept in the table because the issue's design
//! anticipated it; if a future in-scope symbol fails to link directly, it
//! would be added here as a `dlltool`/`.def` import library or a thin
//! `cc`-compiled shim.
//!
//! ## No-op on MSVC
//!
//! On `*-pc-windows-msvc` (and on non-Windows hosts) this crate compiles
//! to a thin, side-effect-free library. It changes nothing about the
//! existing build; it exists so the GNU path has a single, testable place
//! that proves the surface links.

#![cfg_attr(docsrs, feature(doc_cfg))]

#[cfg(target_os = "windows")]
pub mod conpty;

/// `true` iff this crate was compiled for a GNU-ABI Windows target
/// (`*-pc-windows-gnu`).
///
/// The bridge's entire reason to exist is that build; on MSVC and on
/// non-Windows hosts it is an inert no-op. Exposed so consumers and tests
/// can assert which ABI they linked against.
pub const fn is_gnu_windows_target() -> bool {
    cfg!(all(target_os = "windows", target_env = "gnu"))
}

/// Short human-readable label for the ABI this crate linked against:
/// `"windows-gnu"`, `"windows-msvc"`, `"windows-other"`, or
/// `"non-windows"`. Purely informational (diagnostics / test output).
pub const fn target_abi_label() -> &'static str {
    if cfg!(not(target_os = "windows")) {
        "non-windows"
    } else if cfg!(target_env = "gnu") {
        "windows-gnu"
    } else if cfg!(target_env = "msvc") {
        "windows-msvc"
    } else {
        "windows-other"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gnu_flag_matches_cfg() {
        assert_eq!(
            is_gnu_windows_target(),
            cfg!(all(target_os = "windows", target_env = "gnu"))
        );
    }

    #[test]
    fn abi_label_is_consistent() {
        let label = target_abi_label();
        if cfg!(not(target_os = "windows")) {
            assert_eq!(label, "non-windows");
        } else {
            assert!(label.starts_with("windows-"));
            assert_eq!(is_gnu_windows_target(), label == "windows-gnu");
        }
    }

    /// On Windows, referencing the imported ConPTY entry points forces
    /// the linker to bind them from the `windows-sys` import library;
    /// non-null addresses prove the ConPTY surface linked (on `-gnu`,
    /// from the bundled `-gnu` import lib). Does not spawn a console.
    #[cfg(target_os = "windows")]
    #[test]
    fn conpty_entry_points_are_bound() {
        let addrs = conpty::entry_point_addresses();
        assert!(
            addrs.iter().all(|&a| a != 0),
            "ConPTY entry points must resolve to non-null addresses"
        );
    }
}
