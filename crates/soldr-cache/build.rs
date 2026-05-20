fn main() {
    println!("cargo:rerun-if-changed=proto/manifest.proto");
    prost_build::Config::new()
        .compile_protos(&["proto/manifest.proto"], &["proto/"])
        .expect("compile soldr-cache save protos");
}
