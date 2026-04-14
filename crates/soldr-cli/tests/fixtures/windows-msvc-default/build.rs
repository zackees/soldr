fn main() {
    if cfg!(windows) {
        let target = std::env::var("TARGET").expect("missing TARGET");
        if !target.ends_with("windows-msvc") {
            panic!("expected soldr to force windows-msvc, got {target}");
        }

        cc::Build::new()
            .file("native/hello.c")
            .compile("hello");
    }
}
