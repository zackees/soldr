//! `testbin-createfilew-probe` — Windows fixture for #551 slice 7c.
//!
//! Calls `kernel32!CreateFileW` directly with a path the test
//! controls, then exits. Used by
//! `crates/running-process-probe/tests/interposer_integration_windows.rs`
//! to assert the slice 6 detours fire on a real (non-diagnostic)
//! file-open call — Windows analog to the Linux + macOS slice 7d/e
//! tests, which use `/bin/cat` (POSIX `open(2)`) for the same
//! purpose.
//!
//! Why a dedicated fixture: cmd's `type` builtin doesn't appear to
//! go through `kernel32!CreateFileW` (#551 slice 7c investigation),
//! so the slice 7a/7b integration test could only assert that
//! *some* `RPP_HOOK` line fires (the install-thread sentinel). A
//! fixture that explicitly calls `CreateFileW` produces a
//! deterministic `RPP_HOOK file-open path=…` line tied to the
//! probe path, giving the test the same path-specific assertion
//! that the POSIX tests have.
//!
//! ## CLI
//!
//! `testbin-createfilew-probe <delay_ms> <path>`
//!
//! - Sleeps `delay_ms` (host gets time to inject the interposer
//!   before we touch CreateFileW).
//! - Calls `CreateFileW(path, GENERIC_READ, FILE_SHARE_READ, NULL,
//!   OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, NULL)`.
//! - Closes the handle if it's valid, exits 0 either way (the
//!   test only cares that the call went through the detour).

// Non-Windows stub: the testbin is only meaningful on Windows (it
// drives `kernel32!CreateFileW`), but cargo still requires a `main`
// fn on every target so the workspace check + lint passes on
// Linux + macOS. The stub prints a message and exits non-zero so a
// confused caller learns the binary is a no-op outside Windows.
#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!(
        "testbin-createfilew-probe is a Windows-only fixture for #551 slice 7c; \
         this build is a no-op stub."
    );
    std::process::exit(2);
}

#[cfg(target_os = "windows")]
use std::ffi::OsStr;
#[cfg(target_os = "windows")]
use std::os::windows::ffi::OsStrExt;
#[cfg(target_os = "windows")]
use std::time::Duration;

#[cfg(target_os = "windows")]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: {} <delay_ms> <path>", args[0]);
        std::process::exit(2);
    }
    let delay_ms: u64 = args[1]
        .parse()
        .expect("delay_ms must be a positive integer");
    let path = &args[2];

    std::thread::sleep(Duration::from_millis(delay_ms));

    // Wide-encode the path with trailing NUL for CreateFileW.
    let wide: Vec<u16> = OsStr::new(path).encode_wide().chain(Some(0)).collect();

    // Hand-rolled FFI to kernel32!CreateFileW. We can't pull in
    // `windows-sys` here without bloating testbins' dependency
    // tree (and `winapi` would do the same). The signature is
    // stable, well-documented, and ABI-locked: this is the entry
    // we want to detour and the entry our interposer's slice 6b
    // detour patches.
    #[link(name = "kernel32")]
    extern "system" {
        fn CreateFileW(
            lp_file_name: *const u16,
            dw_desired_access: u32,
            dw_share_mode: u32,
            lp_security_attributes: *const core::ffi::c_void,
            dw_creation_disposition: u32,
            dw_flags_and_attributes: u32,
            h_template_file: *mut core::ffi::c_void,
        ) -> *mut core::ffi::c_void;
        fn CloseHandle(h_object: *mut core::ffi::c_void) -> i32;
    }

    const GENERIC_READ: u32 = 0x80000000;
    const FILE_SHARE_READ: u32 = 0x00000001;
    const OPEN_EXISTING: u32 = 3;
    const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
    let invalid_handle: *mut core::ffi::c_void = !0_isize as *mut _;

    unsafe {
        let h = CreateFileW(
            wide.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ,
            core::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            core::ptr::null_mut(),
        );
        if h != invalid_handle && !h.is_null() {
            CloseHandle(h);
        }
    }
}
