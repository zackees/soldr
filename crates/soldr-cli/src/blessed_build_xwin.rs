fn clang_shim_names() -> Vec<String> {
    // Only `clang` + `clang++`. DO NOT add `clang-cl`: the shim invokes
    // it as its downstream, and PATH could then find the shim's own
    // clang-cl first and recurse (ci/docker-aarch64-windows-msvc-cross/).
    vec![
        crate::platform::executable::name::native("clang"),
        crate::platform::executable::name::native("clang++"),
    ]
}

/// Build the MSVC-style include-flag string that cargo-xwin would
/// have injected for a child cargo invocation. Format:
/// `"/imsvc <crt/include> /imsvc <sdk/include/ucrt> ..."` — each
/// `/imsvc` is followed by the path as a separate token. clang-cl
/// natively recognizes `/imsvc <path>` as an MSVC-style include
/// directive (equivalent to clang's `-isystem <path>`).
///
/// Paths that don't exist on disk are silently skipped — defends
/// against catalogue-shape drift (e.g. a future xwin-cache that
/// stops shipping the winrt include tree shouldn't make the whole
/// CFLAGS injection error out).
fn xwin_msvc_cflags(cache_dir: &std::path::Path) -> String {
    let candidates = [
        cache_dir.join("crt").join("include"),
        cache_dir.join("sdk").join("include").join("ucrt"),
        cache_dir.join("sdk").join("include").join("um"),
        cache_dir.join("sdk").join("include").join("shared"),
        cache_dir.join("sdk").join("include").join("winrt"),
        cache_dir.join("sdk").join("include").join("cppwinrt"),
    ];
    candidates
        .iter()
        .filter(|p| p.is_dir())
        // No space between `/imsvc` and the path. clang-cl accepts
        // the `/imsvc<path>` joined form; the two-token `/imsvc <path>`
        // form gets mangled because cc-rs receives CFLAGS, splits on
        // whitespace, then passes each token as a separate argv entry
        // — and clang-cl ends up seeing `/imsvc <path>` as two
        // unrelated args (the path arg is treated as a positional
        // source file). soldr#1070 root cause.
        .map(|p| format!("/imsvc{}", p.display()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Build the rustc lld-link + dynamic CRT flags and
/// `-C link-arg=/LIBPATH:<path>` chain for the xwin-cache. Explicitly
/// select the UCRT/VCRuntime import libraries: the same bundle also ships
/// static `libucrt.lib`, which cannot satisfy `__declspec(dllimport)`
/// references emitted by clang-cl's default `/MD` mode.
///
/// The xwin tarball lays libs out per-arch as `crt/lib/<arch>/` and
/// `sdk/lib/{um,ucrt}/<arch>/` where `<arch>` is the MS arch name
/// (`x64`, `arm64`) — matching xwin's `--preserve-ms-arch-notation`
/// flag in the upstream recipe.
fn xwin_msvc_link_args(cache_dir: &std::path::Path, target_triple: &str) -> String {
    if !target_triple.ends_with("-pc-windows-msvc") {
        return String::new();
    }
    let arch_dirs: &[&str] = if target_triple.starts_with("aarch64-") {
        &["arm64", "aarch64"]
    } else if target_triple.starts_with("x86_64-") {
        &["x64", "x86_64"]
    } else {
        return String::new();
    };
    let mut args = vec![
        "-C".to_string(),
        "linker-flavor=lld-link".to_string(),
        "-C".to_string(),
        "link-arg=/NODEFAULTLIB:libucrt.lib".to_string(),
        "-C".to_string(),
        "link-arg=/DEFAULTLIB:ucrt.lib".to_string(),
        "-C".to_string(),
        "link-arg=/DEFAULTLIB:vcruntime.lib".to_string(),
    ];
    args.extend(
        arch_dirs
            .iter()
            .flat_map(|arch| {
                [
                    cache_dir.join("crt").join("lib").join(arch),
                    cache_dir.join("sdk").join("lib").join("um").join(arch),
                    cache_dir.join("sdk").join("lib").join("ucrt").join(arch),
                ]
            })
            .filter(|p| p.is_dir())
            .flat_map(|p| {
                vec![
                    "-C".to_string(),
                    format!("link-arg=/LIBPATH:{}", p.display()),
                ]
            }),
    );
    args.join(" ")
}
