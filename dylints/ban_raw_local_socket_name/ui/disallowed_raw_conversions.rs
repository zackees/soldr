// aux-build:interprocess.rs

extern crate interprocess;

use interprocess::local_socket::{GenericFilePath, GenericNamespaced, ToFsName, ToNsName};

fn main() {
    let _windows = "resolved-pipe".to_ns_name::<GenericNamespaced>();
    let _unix = "/tmp/resolved.sock".to_fs_name::<GenericFilePath>();
}
