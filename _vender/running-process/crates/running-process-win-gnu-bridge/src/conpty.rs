//! Thin safe-ish wrappers over the three ConPTY entry points, bound
//! **directly** from `windows-sys` (no `GetProcAddress`).
//!
//! The direct import is the whole point. Unlike
//! `running-process::pty::conpty_passthrough::conpty_api`, which resolves
//! the symbols dynamically at runtime (to switch between `kernel32.dll`
//! and a sidecar `conpty.dll`), this crate imports them statically so the
//! **linker** must bind them at build time. That is exactly what proves
//! GNU-linkability: `cargo build --target x86_64-pc-windows-gnu` resolves
//! `CreatePseudoConsole` & friends from the import library `windows-sys`
//! bundles for the `-gnu` target — no Windows SDK, no MSVC `link.exe`.
//!
//! These wrappers are a linkability proof, not a second ConPTY runtime.
//! Production ConPTY dispatch stays in `running-process`; the surface is
//! duplicated here only to give the GNU path a testable binding site.

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Console::{
    ClosePseudoConsole, CreatePseudoConsole, ResizePseudoConsole, COORD, HPCON,
};

/// Windows `HRESULT` (`i32`). Declared locally to avoid pulling the
/// `Win32_System_Com` feature tree in just for the alias (mirrors
/// `running-process`'s `conpty_api`).
pub type HResult = i32;

/// `S_OK` — the success `HRESULT`.
pub const S_OK: HResult = 0;

/// Create a pseudoconsole. Wraps [`CreatePseudoConsole`].
///
/// Returns the opened [`HPCON`] on success, or the failing `HRESULT`.
///
/// # Safety
///
/// `input` and `output` must be valid pipe handles that remain valid for
/// the lifetime of the returned [`HPCON`]. The caller owns the returned
/// handle and must release it with [`close`].
pub unsafe fn create(
    size: COORD,
    input: HANDLE,
    output: HANDLE,
    flags: u32,
) -> Result<HPCON, HResult> {
    // `HPCON` is `isize` in windows-sys 0.59 (an opaque handle), not a
    // pointer — initialize to 0, not `null_mut()`.
    let mut hpc: HPCON = 0;
    let hr = CreatePseudoConsole(size, input, output, flags, &mut hpc);
    if hr == S_OK {
        Ok(hpc)
    } else {
        Err(hr)
    }
}

/// Resize an existing pseudoconsole. Wraps [`ResizePseudoConsole`].
///
/// # Safety
///
/// `hpc` must be a live handle previously returned by [`create`].
pub unsafe fn resize(hpc: HPCON, size: COORD) -> Result<(), HResult> {
    let hr = ResizePseudoConsole(hpc, size);
    if hr == S_OK {
        Ok(())
    } else {
        Err(hr)
    }
}

/// Close a pseudoconsole. Wraps [`ClosePseudoConsole`].
///
/// # Safety
///
/// `hpc` must be a live handle previously returned by [`create`] and must
/// not be used afterwards.
pub unsafe fn close(hpc: HPCON) {
    ClosePseudoConsole(hpc);
}

/// Addresses of the three imported ConPTY entry points.
///
/// Referencing the symbols (rather than calling them) is enough to require
/// the linker to bind them — which is precisely the GNU-linkability proof
/// this crate exists to provide. Used by the smoke test, which must not
/// actually spawn a console.
pub fn entry_point_addresses() -> [usize; 3] {
    [
        CreatePseudoConsole as *const () as usize,
        ResizePseudoConsole as *const () as usize,
        ClosePseudoConsole as *const () as usize,
    ]
}
