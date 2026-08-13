// Private fn cfg, cfg! in a private fn, cfg_attr on a let, and cfg-gated
// use statements — all invisible to the old public-function lint, all
// violations here.

#[cfg(target_os = "windows")] //~ ERROR host-platform selection outside the soldr-platform boundary
fn private_cfg_selection() -> u8 {
    1
}

#[cfg(not(target_os = "windows"))] //~ ERROR host-platform selection outside the soldr-platform boundary
fn private_cfg_selection() -> u8 {
    2
}

fn cfg_macro_in_private_body() -> u8 {
    if cfg!(unix) { 1 } else { 2 } //~ ERROR host-platform selection outside the soldr-platform boundary
}

#[cfg(unix)] //~ ERROR host-platform selection outside the soldr-platform boundary
use std::os::unix::ffi::OsStrExt as _;

pub fn outer() -> u8 {
    #[cfg_attr(windows, allow(dead_code))] //~ ERROR host-platform selection outside the soldr-platform boundary
    let value = 1;
    value
}
