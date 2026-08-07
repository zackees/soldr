// soldr#2319 fixture build script: compile a C and a C++ translation unit with
// the `cc` crate so the produced binary actually exercises the container's
// gcc/g++ (glibc-2.17 catalogue toolchain), not just rustc.
fn main() {
    cc::Build::new().file("csrc/adder.c").compile("adder");
    cc::Build::new()
        .cpp(true)
        .file("csrc/greeter.cpp")
        .compile("greeter");
    println!("cargo:rerun-if-changed=csrc/adder.c");
    println!("cargo:rerun-if-changed=csrc/greeter.cpp");
}
