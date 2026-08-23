// aux-build:interprocess.rs

extern crate interprocess;

use interprocess::local_socket::{GenericFilePath, GenericNamespaced, ToFsName, ToNsName};

fn main() {
    let _windows = "resolved-pipe".to_ns_name::<GenericNamespaced>();
    // Deliberately not an absolute path. compiletest rewrites paths that match
    // the test build directory, and this fixture used to say
    // `/tmp/resolved.sock` -- which on Linux (where the build dir lives under
    // /tmp) was normalized to `$TEST_BUILD_DIR/resolved.sock` and blessed that
    // way, but on Windows stays literal. That made the baseline pass only on
    // the platform it was blessed on. The lint keys on the `to_fs_name` call,
    // not on the string, so a relative name tests the same thing portably.
    let _unix = "resolved.sock".to_fs_name::<GenericFilePath>();
}
