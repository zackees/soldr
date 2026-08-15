fn main() {
    // Judge by TARGET, not host cfg: this fixture exists to prove the soldr
    // front door forces the msvc target triple, and TARGET is exactly the
    // value under test. (A `soldr_platform` reference was mechanically swept
    // in here once — a standalone fixture crate cannot depend on soldr's
    // internal crates, which broke this fixture's build on every Windows
    // lane while never failing to *compile* in-repo, since fixtures build
    // only at test runtime.)
    let target = std::env::var("TARGET").expect("missing TARGET");
    if target.contains("windows") {
        if !target.ends_with("windows-msvc") {
            panic!("expected soldr to force windows-msvc, got {target}");
        }

        cc::Build::new().file("native/hello.c").compile("hello");
    }
}
