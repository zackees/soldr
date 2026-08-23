fn main() {
    println!("cargo:rerun-if-changed=native/add.c");
    cc::Build::new().file("native/add.c").compile("fixture_native");
}
