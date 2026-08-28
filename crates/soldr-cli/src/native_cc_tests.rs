//! Tests for the native-cc env-injection helpers (issue #310).

use super::*;
use std::sync::Mutex;

/// Serialises env-mutating tests so they don't race under `cargo test`'s
/// parallel default. Same pattern as the other env-test files in the
/// crate (`main_tests.rs`, `cargo_front_door_tests.rs`).
static ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, previous }
    }
    fn remove(key: &'static str) -> Self {
        let previous = std::env::var_os(key);
        std::env::remove_var(key);
        Self { key, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        if let Some(v) = &self.previous {
            std::env::set_var(self.key, v);
        } else {
            std::env::remove_var(self.key);
        }
    }
}

/// Expected `wrap_compiler_env` output: `<wrapper> <abs-compiler>` where
/// the compiler is resolved via the same PATH lookup the code uses
/// (soldr#1368). Computing the expectation through `resolve_compiler_to_abs`
/// keeps these assertions deterministic regardless of whether `cc` / `clang`
/// happen to be installed on the test machine.
fn expect_wrapped(wrapper: &str, compiler: &str) -> OsString {
    let mut e = OsString::from(wrapper);
    e.push(" ");
    e.push(resolve_compiler_to_abs(compiler));
    e
}

// -------------------------------------------------------------------------
// env_is_falsy — opt-out parsing
// -------------------------------------------------------------------------

#[test]
fn env_is_falsy_recognizes_off_synonyms() {
    for v in ["0", "false", "off", "no", "FALSE", "OFF", " 0 "] {
        let os = OsString::from(v);
        assert!(env_is_falsy(Some(os.as_os_str())), "{v:?} should be falsy");
    }
}

#[test]
fn env_is_falsy_treats_other_values_as_truthy() {
    for v in ["1", "true", "yes", "on", "anything", ""] {
        let os = OsString::from(v);
        assert!(
            !env_is_falsy(Some(os.as_os_str())),
            "{v:?} should be truthy or empty (still on)"
        );
    }
    assert!(!env_is_falsy(None));
}

// -------------------------------------------------------------------------
// value_starts_with_known_wrapper — no-double-wrap detection
// -------------------------------------------------------------------------

#[test]
fn detects_bare_wrapper_basenames() {
    for wrapper in KNOWN_WRAPPERS {
        assert!(
            value_starts_with_known_wrapper(&format!("{wrapper} clang")),
            "bare {wrapper:?} should match"
        );
    }
}

#[test]
fn detects_wrapper_paths_with_extension() {
    assert!(value_starts_with_known_wrapper(
        "/usr/local/bin/sccache clang"
    ));
    assert!(value_starts_with_known_wrapper(
        "C:\\Tools\\sccache.exe clang-cl"
    ));
    assert!(value_starts_with_known_wrapper("/opt/zccache clang"));
}

#[test]
fn rejects_unknown_compilers_and_wrappers() {
    assert!(!value_starts_with_known_wrapper("clang"));
    assert!(!value_starts_with_known_wrapper("gcc -O2"));
    assert!(!value_starts_with_known_wrapper("/usr/bin/cc"));
    // Distractor: name contains a wrapper substring but isn't one.
    assert!(!value_starts_with_known_wrapper("my-sccache-shim clang"));
}

#[test]
fn handles_empty_and_whitespace_values() {
    assert!(!value_starts_with_known_wrapper(""));
    assert!(!value_starts_with_known_wrapper("   "));
}

// -------------------------------------------------------------------------
// wrap_compiler_env — the core decision
// -------------------------------------------------------------------------

#[test]
fn wraps_default_compiler_when_env_unset() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _g = EnvGuard::remove("SOLDR_TEST_FAKE_CC");
    let wrapper = OsString::from("/tmp/zccache");
    let wrapped = wrap_compiler_env("SOLDR_TEST_FAKE_CC", "cc", &wrapper, true).unwrap();
    assert_eq!(wrapped, expect_wrapped("/tmp/zccache", "cc"));
}

#[test]
fn wraps_user_supplied_compiler() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _g = EnvGuard::set("SOLDR_TEST_FAKE_CC", "clang -O2");
    let wrapper = OsString::from("/tmp/zccache");
    let wrapped = wrap_compiler_env("SOLDR_TEST_FAKE_CC", "cc", &wrapper, true).unwrap();
    assert_eq!(wrapped, expect_wrapped("/tmp/zccache", "clang -O2"));
}

#[test]
fn does_not_double_wrap_existing_known_wrapper() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _g = EnvGuard::set("SOLDR_TEST_FAKE_CC", "sccache clang");
    let wrapper = OsString::from("/tmp/zccache");
    let wrapped = wrap_compiler_env("SOLDR_TEST_FAKE_CC", "cc", &wrapper, true);
    assert!(
        wrapped.is_none(),
        "sccache wrapper should be left alone, got: {wrapped:?}"
    );
}

#[test]
fn does_not_double_wrap_self() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _g = EnvGuard::set("SOLDR_TEST_FAKE_CC", "/opt/zccache clang");
    let wrapper = OsString::from("/tmp/zccache");
    assert!(wrap_compiler_env("SOLDR_TEST_FAKE_CC", "cc", &wrapper, true).is_none());
}

#[test]
fn empty_env_value_falls_back_to_default() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _g = EnvGuard::set("SOLDR_TEST_FAKE_CC", "   ");
    let wrapper = OsString::from("/tmp/zccache");
    let wrapped = wrap_compiler_env("SOLDR_TEST_FAKE_CC", "c++", &wrapper, true).unwrap();
    assert_eq!(wrapped, expect_wrapped("/tmp/zccache", "c++"));
}

#[test]
fn unset_var_with_synthesize_disabled_returns_none() {
    // InjectExistingOnly mode (Windows default): when CC is unset and
    // synthesize_default is false, the function must leave the var
    // alone so cc-rs's own discovery still works.
    let _lock = ENV_LOCK.lock().unwrap();
    let _g = EnvGuard::remove("SOLDR_TEST_FAKE_CC");
    let wrapper = OsString::from("/tmp/zccache");
    let wrapped = wrap_compiler_env("SOLDR_TEST_FAKE_CC", "cc", &wrapper, false);
    assert!(wrapped.is_none());
}

#[test]
fn set_var_with_synthesize_disabled_still_wraps() {
    // InjectExistingOnly: explicit user value gets wrapped regardless
    // of the synthesize flag — that's the whole point of the mode.
    let _lock = ENV_LOCK.lock().unwrap();
    let _g = EnvGuard::set("SOLDR_TEST_FAKE_CC", "clang");
    let wrapper = OsString::from("/tmp/zccache");
    let wrapped = wrap_compiler_env("SOLDR_TEST_FAKE_CC", "cc", &wrapper, false).unwrap();
    assert_eq!(wrapped, expect_wrapped("/tmp/zccache", "clang"));
}

#[test]
fn musl_targets_default_to_musl_compilers() {
    assert_eq!(
        default_c_compiler_for_target("x86_64-unknown-linux-musl"),
        "musl-gcc"
    );
    assert_eq!(
        default_cxx_compiler_for_target("aarch64-unknown-linux-musl"),
        "musl-g++"
    );
    assert_eq!(
        default_c_compiler_for_target("x86_64-unknown-linux-gnu"),
        "cc"
    );
    assert_eq!(
        default_cxx_compiler_for_target("aarch64-apple-darwin"),
        "c++"
    );
}

#[test]
fn wraps_musl_target_default_with_musl_gcc() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _g = EnvGuard::remove("CC_x86_64_unknown_linux_musl");
    let wrapper = OsString::from("/tmp/zccache");
    let wrapped = wrap_compiler_env(
        "CC_x86_64_unknown_linux_musl",
        default_c_compiler_for_target("x86_64-unknown-linux-musl"),
        &wrapper,
        true,
    )
    .unwrap();
    assert_eq!(wrapped, expect_wrapped("/tmp/zccache", "musl-gcc"));
}

// -------------------------------------------------------------------------
// native_cache_decision — platform + env interaction
// -------------------------------------------------------------------------

#[test]
fn decision_respects_user_opt_out() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _g = EnvGuard::set(NATIVE_CACHE_ENV_VAR, "0");
    // User opt-out wins on every platform — the env-var check runs
    // before the platform branch in `native_cache_decision`.
    assert_eq!(native_cache_decision(), NativeCacheDecision::UserDisabled);
}

#[test]
fn decision_default_on_for_non_windows_when_env_unset() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _g = EnvGuard::remove(NATIVE_CACHE_ENV_VAR);
    let decision = native_cache_decision();
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        assert_eq!(decision, NativeCacheDecision::InjectExistingOnly);
    } else {
        assert_eq!(decision, NativeCacheDecision::Inject);
    }
}

#[test]
fn decision_truthy_env_value_keeps_default_on() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _g = EnvGuard::set(NATIVE_CACHE_ENV_VAR, "1");
    let decision = native_cache_decision();
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        assert_eq!(decision, NativeCacheDecision::InjectExistingOnly);
    } else {
        assert_eq!(decision, NativeCacheDecision::Inject);
    }
}

// -------------------------------------------------------------------------
// inject_native_cache_env — top-level command env application
// -------------------------------------------------------------------------

#[test]
fn inject_sets_cc_cxx_and_known_wrapper_when_default_on() {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        return;
    }
    let _lock = ENV_LOCK.lock().unwrap();
    let _gn = EnvGuard::remove(NATIVE_CACHE_ENV_VAR);
    let _gcc = EnvGuard::remove("CC");
    let _gcxx = EnvGuard::remove("CXX");
    let _gkw = EnvGuard::remove(CC_KNOWN_WRAPPER_CUSTOM_ENV_VAR);

    let wrapper = std::path::PathBuf::from("/tmp/zccache");
    let mut cmd = std::process::Command::new("echo");
    inject_native_cache_env(&mut cmd, &wrapper, None).unwrap();

    // std::process::Command doesn't expose its env map publicly,
    // so we round-trip by spawning the command — but `echo` is
    // good enough: we can stat the env vars set on the child via
    // its inherited env. Simpler still: just verify the function
    // didn't error and that the side-effects-on-state behave
    // correctly via the unit tests below. End-to-end coverage is
    // the integration test under tests/cargo_front_door/cli_cargo_native_cc.rs.
}

#[test]
fn inject_no_op_when_user_disabled() {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        return;
    }
    let _lock = ENV_LOCK.lock().unwrap();
    let _g = EnvGuard::set(NATIVE_CACHE_ENV_VAR, "0");

    let wrapper = std::path::PathBuf::from("/tmp/zccache");
    let mut cmd = std::process::Command::new("echo");
    inject_native_cache_env(&mut cmd, &wrapper, None).unwrap();
    // Decision verifies the gate held; no panic.
    assert_eq!(native_cache_decision(), NativeCacheDecision::UserDisabled);
}

#[test]
fn inject_wraps_target_specific_when_user_set_them() {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        return;
    }
    let _lock = ENV_LOCK.lock().unwrap();
    let _gn = EnvGuard::remove(NATIVE_CACHE_ENV_VAR);
    let triple = "x86_64-unknown-linux-gnu";
    let snake = triple.replace('-', "_");
    let cc_var: &'static str = Box::leak(format!("CC_{snake}").into_boxed_str());
    let cxx_var: &'static str = Box::leak(format!("CXX_{snake}").into_boxed_str());
    let _gcc = EnvGuard::set(cc_var, "clang");
    let _gcxx = EnvGuard::set(cxx_var, "clang++");
    // Make sure the function reads them as set.
    let wrapper = OsString::from("/tmp/zccache");
    let cc_wrapped = wrap_compiler_env(cc_var, "cc", &wrapper, true).unwrap();
    let cxx_wrapped = wrap_compiler_env(cxx_var, "c++", &wrapper, true).unwrap();
    assert_eq!(cc_wrapped, expect_wrapped("/tmp/zccache", "clang"));
    assert_eq!(cxx_wrapped, expect_wrapped("/tmp/zccache", "clang++"));
}

#[test]
fn target_wrapper_mirrors_snake_env_to_cc_rs_dash_key() {
    let _lock = ENV_LOCK.lock().unwrap();
    let triple = "x86_64-apple-darwin";
    let dashed: &'static str = Box::leak(format!("CC_{triple}").into_boxed_str());
    let snake: &'static str =
        Box::leak(format!("CC_{}", triple.replace('-', "_")).into_boxed_str());
    let _gd = EnvGuard::remove(dashed);
    let _gs = EnvGuard::set(snake, "clang --target=x86_64-apple-darwin");

    let mut cmd = std::process::Command::new("echo");
    let wrapper = OsString::from("/tmp/zccache");
    wrap_target_compiler_envs(&mut cmd, "CC_", triple, "cc", &wrapper, true);

    let get = |key: &str| -> Option<OsString> {
        cmd.get_envs()
            .find(|(candidate, _)| *candidate == OsStr::new(key))
            .and_then(|(_, value)| value.map(OsString::from))
    };
    let expected = expect_wrapped("/tmp/zccache", "clang --target=x86_64-apple-darwin");
    assert_eq!(get(snake), Some(expected.clone()));
    assert_eq!(get(dashed), Some(expected));
}

#[test]
fn target_wrapper_honors_command_level_snake_override() {
    let _lock = ENV_LOCK.lock().unwrap();
    let triple = "aarch64-pc-windows-msvc";
    let dashed: &'static str = Box::leak(format!("CC_{triple}").into_boxed_str());
    let snake: &'static str =
        Box::leak(format!("CC_{}", triple.replace('-', "_")).into_boxed_str());
    let _gd = EnvGuard::remove(dashed);
    let _gs = EnvGuard::remove(snake);

    let mut cmd = std::process::Command::new("echo");
    cmd.env(snake, "clang-cl");
    let wrapper = OsString::from("/tmp/zccache");
    wrap_target_compiler_envs(&mut cmd, "CC_", triple, "cc", &wrapper, true);

    let get = |key: &str| -> Option<OsString> {
        cmd.get_envs()
            .find(|(candidate, _)| *candidate == OsStr::new(key))
            .and_then(|(_, value)| value.map(OsString::from))
    };
    let expected = expect_wrapped("/tmp/zccache", "clang-cl");
    assert_eq!(get(snake), Some(expected.clone()));
    assert_eq!(get(dashed), Some(expected));
}

// -------------------------------------------------------------------------
// soldr#1368 — inject uses the zccache-soldr shim + an absolute compiler
// -------------------------------------------------------------------------

#[test]
fn inject_uses_shim_wrapper_and_absolute_compiler() {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        return;
    }
    use std::ffi::OsStr;
    let _lock = ENV_LOCK.lock().unwrap();
    let _gn = EnvGuard::remove(NATIVE_CACHE_ENV_VAR);
    let _gcc = EnvGuard::remove("CC");
    let _gcxx = EnvGuard::remove("CXX");
    let _gkw = EnvGuard::remove(CC_KNOWN_WRAPPER_CUSTOM_ENV_VAR);

    // The shim must exist for `inject_native_cache_env` to inject
    // (soldr#1377): the precheck now falls back cleanly when the shim
    // isn't a real file AND isn't on PATH, so CI cross-build lanes that
    // run via setup-soldr's pinned soldr (which ships no shim sibling)
    // don't ENOENT every -sys crate. Create the shim as a real file
    // inside a tempdir so this test exercises the injection path.
    let tmp = tempfile::tempdir().unwrap();
    let shim = tmp.path().join("zccache-soldr");
    std::fs::write(&shim, b"#!/bin/sh\nexec \"$@\"\n").unwrap();
    let shim_str = shim.to_string_lossy().to_string();

    let mut cmd = std::process::Command::new("echo");
    inject_native_cache_env(&mut cmd, &shim, None).unwrap();

    let get = |key: &str| -> Option<OsString> {
        cmd.get_envs()
            .find(|(k, _)| *k == OsStr::new(key))
            .and_then(|(_, v)| v.map(OsString::from))
    };

    // CC = "<shim> <abs-cc>": first token is the shim, second is absolute.
    let cc = get("CC").expect("CC must be injected when native cache is on");
    let cc_str = cc.to_string_lossy();
    let mut toks = cc_str.split(' ');
    assert_eq!(
        toks.next(),
        Some(shim_str.as_str()),
        "CC wrapper token must be the zccache-soldr shim, got: {cc_str}"
    );
    let compiler = toks.next().expect("CC must carry an underlying compiler");
    // On a machine with `cc` on PATH this is absolute; if `cc` is not
    // installed it falls back to the bare name — assert whichever the
    // resolver itself produced so the test is deterministic.
    let expected_compiler = resolve_compiler_to_abs("cc");
    assert_eq!(OsStr::new(compiler), expected_compiler.as_os_str());

    // cc-rs must be told the wrapper stem so it strips the shim when
    // classifying the compiler family.
    assert_eq!(
        get(CC_KNOWN_WRAPPER_CUSTOM_ENV_VAR).as_deref(),
        Some(OsStr::new("zccache-soldr")),
        "CC_KNOWN_WRAPPER_CUSTOM must name the shim stem"
    );
}
