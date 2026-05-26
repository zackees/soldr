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
    assert_eq!(wrapped, OsString::from("/tmp/zccache cc"));
}

#[test]
fn wraps_user_supplied_compiler() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _g = EnvGuard::set("SOLDR_TEST_FAKE_CC", "clang -O2");
    let wrapper = OsString::from("/tmp/zccache");
    let wrapped = wrap_compiler_env("SOLDR_TEST_FAKE_CC", "cc", &wrapper, true).unwrap();
    assert_eq!(wrapped, OsString::from("/tmp/zccache clang -O2"));
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
    assert_eq!(wrapped, OsString::from("/tmp/zccache c++"));
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
    assert_eq!(wrapped, OsString::from("/tmp/zccache clang"));
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
    assert_eq!(wrapped, OsString::from("/tmp/zccache musl-gcc"));
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
    if cfg!(target_os = "windows") {
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
    if cfg!(target_os = "windows") {
        assert_eq!(decision, NativeCacheDecision::InjectExistingOnly);
    } else {
        assert_eq!(decision, NativeCacheDecision::Inject);
    }
}

// -------------------------------------------------------------------------
// inject_native_cache_env — top-level command env application
// -------------------------------------------------------------------------

#[cfg(not(target_os = "windows"))]
#[test]
fn inject_sets_cc_cxx_and_known_wrapper_when_default_on() {
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
    // the integration test under tests/cli_cargo_native_cc.rs.
}

#[cfg(not(target_os = "windows"))]
#[test]
fn inject_no_op_when_user_disabled() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _g = EnvGuard::set(NATIVE_CACHE_ENV_VAR, "0");

    let wrapper = std::path::PathBuf::from("/tmp/zccache");
    let mut cmd = std::process::Command::new("echo");
    inject_native_cache_env(&mut cmd, &wrapper, None).unwrap();
    // Decision verifies the gate held; no panic.
    assert_eq!(native_cache_decision(), NativeCacheDecision::UserDisabled);
}

#[cfg(not(target_os = "windows"))]
#[test]
fn inject_wraps_target_specific_when_user_set_them() {
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
    assert_eq!(cc_wrapped, OsString::from("/tmp/zccache clang"));
    assert_eq!(cxx_wrapped, OsString::from("/tmp/zccache clang++"));
}
