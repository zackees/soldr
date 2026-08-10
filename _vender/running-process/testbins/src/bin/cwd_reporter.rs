//! Test binary: writes its current working directory to stdout in UTF-8
//! and exits.
//!
//! Unlike `cmd.exe`'s `cd` builtin (which on Windows emits paths in the
//! console's active codepage and corrupts non-ASCII characters), this
//! probe goes through `std::env::current_dir()` and prints the raw
//! UTF-8 bytes, so adversarial tests can round-trip Unicode cwd paths
//! through `spawn` without the shell layer mangling them.

use std::io::Write;

fn main() {
    let cwd = std::env::current_dir().expect("current_dir");
    // PathBuf::display() lossy-converts to UTF-8 — fine for ASCII +
    // Unicode-in-OsStr paths. On Unix paths that are not valid UTF-8
    // we'd lose information, but those don't arise in our tests.
    let bytes = cwd.to_string_lossy().into_owned().into_bytes();
    std::io::stdout()
        .write_all(&bytes)
        .expect("write cwd to stdout");
    std::io::stdout().flush().unwrap();
}
