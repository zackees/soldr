// soldr#2319 fixture binary: calls into the C and C++ objects compiled by
// build.rs, so the final ELF genuinely links native C/C++ code.
extern "C" {
    fn soldr_fixture_add(a: i32, b: i32) -> i32;
    fn soldr_fixture_len(s: *const u8) -> i32;
}

fn main() {
    // SAFETY: the C implementation is compiled by this fixture's build.rs,
    // and the declared `extern "C"` signature exactly matches it.
    let sum = unsafe { soldr_fixture_add(40, 2) };
    // SAFETY: the C++ implementation is compiled by this fixture's build.rs;
    // the byte string is NUL-terminated and stays live for the call.
    let len = unsafe { soldr_fixture_len(b"soldr\0".as_ptr()) };
    println!("sum={sum} len={len}");
    assert_eq!(sum, 42);
    assert_eq!(len, 5);
}
