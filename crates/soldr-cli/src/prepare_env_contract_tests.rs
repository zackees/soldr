//! Precedence contract for the flags `soldr prepare --github-env` exports.
//!
//! Lives in its own module rather than in `prepare_cmd`'s test block because
//! that file is already over the LOC ratchet's ceiling and may not grow.
//!
//! ## What is being pinned
//!
//! `apply_blessed_prep_env` exports `CARGO_ENCODED_RUSTFLAGS`, which outranks
//! both `RUSTFLAGS` and `CARGO_TARGET_<triple>_RUSTFLAGS` in Cargo's
//! precedence order. Whatever it writes there is therefore the *only* thing
//! that takes effect, so anything the caller had already put in the
//! lower-precedence variables has to be folded in rather than shadowed.
//!
//! `apply_to_process` has covered the in-process half of this since
//! `applying_target_flags_consumes_higher_precedence_globals`
//! (`target_lifecycle`). The `--github-env` half — the one CI actually runs —
//! had no equivalent.
//!
//! zackees/clud#732 is why this is worth a test: a bump that moved the MSVC
//! link configuration into the encoded variable cost that consumer a CI cycle,
//! because the precedence rule was not written down and its guard assumed the
//! target-scoped key still won.

use crate::blessed_build::BlessedPrep;
use crate::prepare_cmd::apply_blessed_prep_env;
use crate::{EnvVarGuard, TEST_PROCESS_ENV_LOCK};

fn write_executable(path: &std::path::Path, body: &str) {
    std::fs::write(path, body).expect("write fake executable");
    let source = std::fs::metadata(path)
        .expect("stat fake executable")
        .permissions();
    crate::platform::fs::permissions::make_executable_from(path, &source)
        .expect("chmod fake executable");
}

struct DynamicEnvVarGuard {
    key: String,
    previous: Option<std::ffi::OsString>,
}

impl DynamicEnvVarGuard {
    fn remove(key: String) -> Self {
        let previous = std::env::var_os(&key);
        std::env::remove_var(&key);
        Self { key, previous }
    }
}

impl Drop for DynamicEnvVarGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var(&self.key, value),
            None => std::env::remove_var(&self.key),
        }
    }
}

#[test]
fn managed_gnu_toolchain_is_exported_for_later_github_steps() {
    if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Linux {
        return;
    }
    let _lock = TEST_PROCESS_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());

    let (target, target_prefix, slug) = if crate::platform::host::facts::arch()
        == crate::platform::host::facts::HostArch::Aarch64
    {
        (
            "x86_64-unknown-linux-gnu",
            "x86_64-conda-linux-gnu",
            "linux-x64-gnu",
        )
    } else {
        (
            "aarch64-unknown-linux-gnu",
            "aarch64-conda-linux-gnu",
            "linux-arm64-gnu",
        )
    };
    let target_u = target.replace('-', "_");
    let target_upper = target_u.to_ascii_uppercase();
    let output_keys = [
        format!("CC_{target_u}"),
        format!("CXX_{target_u}"),
        format!("CXXSTDLIB_{target_u}"),
        "CXXSTDLIB".to_string(),
        format!("AR_{target_u}"),
        format!("RANLIB_{target_u}"),
        format!("CFLAGS_{target_u}"),
        format!("CXXFLAGS_{target_u}"),
        format!("CARGO_TARGET_{target_upper}_LINKER"),
        format!("CARGO_TARGET_{target_upper}_RUSTFLAGS"),
        "CMAKE_SYSROOT".to_string(),
        // soldr#3081: the managed sysroot is exported to the compiler and to
        // CMake, but never to pkg-config as a path-rewriting prefix.
        format!("PKG_CONFIG_ALLOW_CROSS_{target}"),
        "PKG_CONFIG_LIBDIR".to_string(),
        "SOLDR_GNU_LINUX_SYSROOT".to_string(),
        "SOLDR_GNU_LINUX_TOOLCHAIN_ROOT".to_string(),
    ];

    let dir = tempfile::tempdir().expect("tempdir");
    let fake_bin = dir.path().join("fake-bin");
    std::fs::create_dir_all(&fake_bin).expect("create fake bin");
    let fake_rustup = fake_bin.join("rustup");
    write_executable(&fake_rustup, "#!/bin/sh\nexit 0\n");

    let _rustup = EnvVarGuard::set(crate::TEST_RUSTUP_BIN_ENV_VAR, &fake_rustup);
    // soldr#2874: this fixture seeds a FAKE bundle and asserts the env
    // contract, so it must reach the catalogue path on every host. Both
    // branches above are deliberately a cross, and on a native ARM64 runner
    // the x86_64-hosted bundle genuinely cannot execute -- selection now
    // refuses it, which is the fix working. The seam says "this host is
    // pretending it can run the Linux bundle", which is exactly what a fake
    // bundle made of `#!/bin/sh` stubs is doing.
    let _cross_guard = EnvVarGuard::set("SOLDR_WINDOWS_LINUX_CROSS_GUARD", "off");
    let _no_network = EnvVarGuard::set("SOLDR_TEST_NO_NETWORK", "1");
    let _legacy_sys = EnvVarGuard::set(crate::blessed_build::USE_LEGACY_VENDORED_SYS_ENV_VAR, "1");
    let _system_cmake = EnvVarGuard::set(crate::blessed_build::USE_SYSTEM_CMAKE_ENV_VAR, "1");
    let _manifest = EnvVarGuard::set("SOLDR_MANIFEST_DISABLE", "1");
    let _path = EnvVarGuard::set("PATH", "/usr/bin:/bin");
    let _ambient_cc = EnvVarGuard::set("CC", "ambient-cc");
    let _ambient_cxx = EnvVarGuard::set("CXX", "ambient-cxx");
    let _ambient_ar = EnvVarGuard::set("AR", "ambient-ar");
    let _ambient_ranlib = EnvVarGuard::set("RANLIB", "ambient-ranlib");
    // soldr#2309: the stdlib pin is setdefault — an ambient value through any
    // spelling in cc-rs's lookup chain suppresses it, so the whole chain must
    // be cleared for the injection half of this test. Deduplicated: guarding
    // the same key twice would restore it twice, last writer wins, and the
    // second guard captured the already-removed state.
    let _output_guards: Vec<_> = output_keys
        .iter()
        .cloned()
        .chain(crate::fetch::gnu_linux_toolchain::cxx_stdlib_lookup_keys(
            target,
        ))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .map(DynamicEnvVarGuard::remove)
        .collect();

    let paths = crate::core::SoldrPaths::with_root(dir.path().join("soldr"));
    let bundle = paths
        .bin
        .join("syslib")
        .join("gnu-linux-toolchain")
        .join(crate::fetch::gnu_linux_toolchain::GNU_LINUX_TOOLCHAIN_VERSION)
        .join(slug);
    let package = bundle.join("package");
    let managed_bin = package.join("bin");
    let sysroot = package.join(target_prefix).join("sysroot");
    std::fs::create_dir_all(&managed_bin).expect("create managed bin");
    std::fs::create_dir_all(sysroot.join("usr/include")).expect("create sysroot includes");
    std::fs::create_dir_all(sysroot.join("usr/lib")).expect("create sysroot libraries");
    for tool in ["gcc", "g++", "ar", "ranlib", "ld", "readelf"] {
        write_executable(
            &managed_bin.join(format!("{target_prefix}-{tool}")),
            "#!/bin/sh\nexit 0\n",
        );
    }
    std::fs::write(bundle.join(".complete"), "test bundle").expect("write bundle stamp");

    let prep = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(crate::target_lifecycle::prepare_target(&paths, target))
        .expect("prepare managed GNU target");
    assert!(
        prep.path_dirs.contains(&managed_bin),
        "managed GNU bin directory missing from prepared PATH entries: {:?}",
        prep.path_dirs
    );

    let github_env = dir.path().join("github.env");
    apply_blessed_prep_env(Some(&github_env), &prep, target).expect("export prepared env");

    let process_path = std::env::split_paths(&std::env::var_os("PATH").expect("process PATH"))
        .next()
        .expect("first process PATH entry");
    assert_eq!(process_path, managed_bin);

    let exported = std::fs::read_to_string(&github_env).expect("read github env");
    let exported_path = exported
        .lines()
        .find_map(|line| line.strip_prefix("PATH="))
        .expect("PATH was not exported");
    let first_exported = std::env::split_paths(std::ffi::OsStr::new(exported_path))
        .next()
        .expect("first exported PATH entry");
    assert_eq!(first_exported, managed_bin);

    for key in &output_keys {
        let process_value = std::env::var(key).unwrap_or_else(|_| panic!("{key} not applied"));
        assert!(
            exported
                .lines()
                .any(|line| line == format!("{key}={process_value}")),
            "{key} was not exported to GITHUB_ENV"
        );
    }
    assert_eq!(
        std::env::var(format!("CC_{target_u}")).expect("C compiler"),
        managed_bin
            .join(format!("{target_prefix}-gcc"))
            .to_string_lossy()
    );
    assert_eq!(
        std::env::var("SOLDR_GNU_LINUX_SYSROOT").expect("sysroot"),
        sysroot.to_string_lossy()
    );
    // soldr#2309: both spellings of the C++ stdlib pin point at libstdc++ —
    // the only C++ runtime the catalogue GNU driver ships.
    assert_eq!(
        std::env::var(format!("CXXSTDLIB_{target_u}")).expect("target-scoped stdlib pin"),
        "stdc++"
    );
    assert_eq!(
        std::env::var("CXXSTDLIB").expect("bare stdlib pin on a Linux host"),
        "stdc++"
    );

    // Setdefault: with CXXSTDLIB now present in the process env (exported by
    // the first preparation above), a re-preparation must treat it as a
    // caller decision and inject no stdlib pin at all.
    let reprep = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(crate::target_lifecycle::prepare_target(&paths, target))
        .expect("re-prepare managed GNU target");
    assert!(
        !reprep
            .env
            .iter()
            .any(|(key, _)| key.starts_with("CXXSTDLIB")),
        "a caller-set CXXSTDLIB must suppress the pin: {:?}",
        reprep.env
    );

    for (alias, ambient) in [
        ("CC", "ambient-cc"),
        ("CXX", "ambient-cxx"),
        ("AR", "ambient-ar"),
        ("RANLIB", "ambient-ranlib"),
    ] {
        assert!(
            !exported
                .lines()
                .any(|line| line.starts_with(&format!("{alias}="))),
            "cross-target {alias} must not leak into later Cargo host builds: {exported}"
        );
        assert_eq!(
            std::env::var_os(alias).as_deref(),
            Some(std::ffi::OsStr::new(ambient)),
            "{alias} must retain its ambient value in Soldr's process"
        );
    }
}

#[test]
fn managed_musl_toolchain_is_exported_without_zig_or_host_tools() {
    if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Linux {
        return;
    }
    let _lock = TEST_PROCESS_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());

    let (target, target_prefix, slug) = if crate::platform::host::facts::arch()
        == crate::platform::host::facts::HostArch::Aarch64
    {
        (
            "x86_64-unknown-linux-musl",
            "x86_64-linux-musl",
            "linux-x64-musl",
        )
    } else {
        (
            "aarch64-unknown-linux-musl",
            "aarch64-linux-musl",
            "linux-arm64-musl",
        )
    };
    let target_u = target.replace('-', "_");
    let target_upper = target_u.to_ascii_uppercase();
    let output_keys = [
        format!("CC_{target_u}"),
        format!("CXX_{target_u}"),
        format!("AR_{target_u}"),
        format!("RANLIB_{target_u}"),
        format!("CFLAGS_{target_u}"),
        format!("CXXFLAGS_{target_u}"),
        format!("CARGO_TARGET_{target_upper}_LINKER"),
        format!("CARGO_TARGET_{target_upper}_RUSTFLAGS"),
        "CMAKE_SYSROOT".to_string(),
        // soldr#3081: the managed sysroot is exported to the compiler and to
        // CMake, but never to pkg-config as a path-rewriting prefix.
        format!("PKG_CONFIG_ALLOW_CROSS_{target}"),
        "PKG_CONFIG_LIBDIR".to_string(),
        "SOLDR_MUSL_LINUX_SYSROOT".to_string(),
        "SOLDR_MUSL_LINUX_TOOLCHAIN_ROOT".to_string(),
    ];

    let dir = tempfile::tempdir().expect("tempdir");
    let fake_bin = dir.path().join("fake-bin");
    std::fs::create_dir_all(&fake_bin).expect("create fake bin");
    let fake_rustup = fake_bin.join("rustup");
    write_executable(&fake_rustup, "#!/bin/sh\nexit 0\n");

    let _rustup = EnvVarGuard::set(crate::TEST_RUSTUP_BIN_ENV_VAR, &fake_rustup);
    let _no_network = EnvVarGuard::set("SOLDR_TEST_NO_NETWORK", "1");
    let _legacy_sys = EnvVarGuard::set(crate::blessed_build::USE_LEGACY_VENDORED_SYS_ENV_VAR, "1");
    let _system_cmake = EnvVarGuard::set(crate::blessed_build::USE_SYSTEM_CMAKE_ENV_VAR, "1");
    let _manifest = EnvVarGuard::set("SOLDR_MANIFEST_DISABLE", "1");
    let _path = EnvVarGuard::set("PATH", "/usr/bin:/bin");
    let _output_guards: Vec<_> = output_keys
        .iter()
        .cloned()
        .map(DynamicEnvVarGuard::remove)
        .collect();

    let paths = crate::core::SoldrPaths::with_root(dir.path().join("soldr"));
    let bundle = paths
        .bin
        .join("syslib")
        .join("musl-linux-toolchain")
        .join(crate::fetch::musl_linux_toolchain::MUSL_LINUX_TOOLCHAIN_VERSION)
        .join(slug);
    let package = bundle.join("package");
    let managed_bin = package.join("bin");
    let sysroot = package.join(target_prefix);
    std::fs::create_dir_all(&managed_bin).expect("create managed bin");
    std::fs::create_dir_all(sysroot.join("include")).expect("create musl headers");
    std::fs::create_dir_all(sysroot.join("lib")).expect("create musl libraries");
    for tool in [
        "gcc", "g++", "ar", "ranlib", "ld", "readelf", "strip", "objcopy",
    ] {
        write_executable(
            &managed_bin.join(format!("{target_prefix}-{tool}")),
            "#!/bin/sh\nexit 0\n",
        );
    }
    for runtime in [
        "crt1.o",
        "rcrt1.o",
        "crti.o",
        "crtn.o",
        "libc.a",
        "libstdc++.a",
    ] {
        std::fs::write(sysroot.join("lib").join(runtime), "managed musl runtime")
            .expect("write managed musl runtime");
    }
    std::fs::write(bundle.join(".complete"), "test bundle").expect("write bundle stamp");

    let prep = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(crate::target_lifecycle::prepare_target(&paths, target))
        .expect("prepare managed musl target");
    assert!(prep.path_dirs.contains(&managed_bin));
    assert!(
        !prep
            .path_dirs
            .iter()
            .any(|path| path.to_string_lossy().contains("zig")),
        "normal musl preparation must not add a Zig directory: {:?}",
        prep.path_dirs
    );
    let github_env = dir.path().join("github.env");
    apply_blessed_prep_env(Some(&github_env), &prep, target).expect("export prepared env");
    let exported = std::fs::read_to_string(&github_env).expect("read github env");
    for key in &output_keys {
        let process_value = std::env::var(key).unwrap_or_else(|_| panic!("{key} not applied"));
        assert!(exported
            .lines()
            .any(|line| line == format!("{key}={process_value}")));
    }
    assert_eq!(
        std::env::var(format!("CC_{target_u}")).expect("C compiler"),
        managed_bin
            .join(format!("{target_prefix}-gcc"))
            .to_string_lossy()
    );
    assert_eq!(
        std::env::var("SOLDR_MUSL_LINUX_SYSROOT").expect("sysroot"),
        sysroot.to_string_lossy()
    );
    // soldr#2309: the C++ stdlib pin is a linux-gnu concern — the musl
    // lifecycle must stay unchanged.
    assert!(
        !exported.lines().any(|line| line.starts_with("CXXSTDLIB")),
        "musl preparation must not export a C++ stdlib pin: {exported}"
    );
}

#[test]
fn exported_encoded_rustflags_keep_caller_target_flags() {
    let _lock = TEST_PROCESS_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let target_key = "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS";
    let _target = EnvVarGuard::set(target_key, "-C link-arg=advapi32.lib");
    let _global = EnvVarGuard::set("RUSTFLAGS", "-Dwarnings");
    let _encoded = EnvVarGuard::remove("CARGO_ENCODED_RUSTFLAGS");

    let mut prep = BlessedPrep::default();
    prep.env.push((
        target_key.to_string(),
        "-C link-arg=/LIBPATH:/soldr/sdk".to_string(),
    ));

    let dir = tempfile::tempdir().expect("tempdir");
    let github_env = dir.path().join("github.env");
    apply_blessed_prep_env(Some(&github_env), &prep, "x86_64-pc-windows-msvc")
        .expect("apply prep env");

    let exported = std::fs::read_to_string(&github_env).expect("read github env");
    let encoded_line = exported
        .lines()
        .find_map(|line| line.strip_prefix("CARGO_ENCODED_RUSTFLAGS="))
        .expect("CARGO_ENCODED_RUSTFLAGS was not exported");
    let tokens: Vec<&str> = encoded_line.split('\u{1f}').collect();

    // soldr's own required SDK flags.
    assert!(
        tokens.contains(&"link-arg=/LIBPATH:/soldr/sdk"),
        "required SDK flag missing from {tokens:?}"
    );
    // The caller's target-scoped flag, which the encoded variable would
    // otherwise shadow into oblivion.
    assert!(
        tokens.contains(&"link-arg=advapi32.lib"),
        "caller's target-scoped flag was dropped from {tokens:?}"
    );
    // And the lower-precedence global.
    assert!(
        tokens.contains(&"-Dwarnings"),
        "caller's global RUSTFLAGS was dropped from {tokens:?}"
    );
}
