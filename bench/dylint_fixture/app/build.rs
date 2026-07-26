fn main() {
    // Deliberately simple: this fixture only needs a build.rs to exist so
    // the bench exercises the same build-script code path real soldr
    // workspaces hit. Nothing here is expensive or platform-specific.
    println!("cargo:rerun-if-changed=src/main.rs");
    println!("cargo:rerun-if-changed=build.rs");
}
