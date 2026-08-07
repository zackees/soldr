use std::os::raw::c_char;

extern "C" {
    fn ccrs_pin_len(s: *const c_char) -> i32;
    fn cmake_pin_len(s: *const c_char) -> i32;
}

fn main() {
    let probe = std::ffi::CString::new("soldr#2309").expect("cstring");
    let (a, b) = unsafe { (ccrs_pin_len(probe.as_ptr()), cmake_pin_len(probe.as_ptr())) };
    assert_eq!(a, b);
    println!("cxx-stdlib-pin ok: {a}");
}
