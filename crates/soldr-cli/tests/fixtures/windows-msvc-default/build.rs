fn main() {
    if matches!(soldr_platform::host::facts::os(), soldr_platform::host::facts::HostOs::Windows) {
        let target = std::env::var("TARGET").expect("missing TARGET");
        if !target.ends_with("windows-msvc") {
            panic!("expected soldr to force windows-msvc, got {target}");
        }

        cc::Build::new()
            .file("native/hello.c")
            .compile("hello");
    }
}
