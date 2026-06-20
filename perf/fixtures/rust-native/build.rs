fn main() {
    cc::Build::new()
        .file("native/checksum.c")
        .warnings(false)
        .compile("rust_native_checksum");
}
