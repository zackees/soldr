//! Test binary: dumps every environment variable to the file path
//! given in argv[1], as `KEY=VALUE\n` lines, then exits.
//!
//! Going through a Rust binary (rather than shell `set` / `printenv`)
//! lets adversarial env-handling tests probe the daemon → child env
//! seam without picking up shell-specific quirks.

use std::io::Write;

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: env-dump <output_path>");
    // Write atomically via tmp + rename so the consumer never sees a
    // half-written file. The consumer polls for `path` to appear, and
    // without rename it would observe the empty/partial file the moment
    // `File::create` returns — racing the loop below. Especially likely
    // under coverage-instrumented test runs where everything's slower.
    let tmp = format!("{path}.tmp");
    {
        let mut out = std::fs::File::create(&tmp).expect("create tmp output file");
        let mut vars: Vec<(String, String)> = std::env::vars().collect();
        vars.sort();
        for (key, value) in vars {
            out.write_all(key.as_bytes()).expect("write key");
            out.write_all(b"=").expect("write =");
            out.write_all(value.as_bytes()).expect("write value");
            out.write_all(b"\n").expect("write newline");
        }
        out.flush().expect("flush");
    }
    std::fs::rename(&tmp, &path).expect("rename tmp -> final");
}
