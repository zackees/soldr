//! soldr#981 Phase 5c contract: the compile env filter.
//!
//! `compile_dispatch::is_compile_env_var` forwards the compile-relevant env
//! (rustc / cc-rs / zccache / MSVC-host (soldr#1079) vars AND arbitrary
//! build-script `cargo:rustc-env` vars — see the linux-arm-musl crgx
//! regression) while dropping known interactive-session noise (`PROMPT`,
//! `DISPLAY`, `XDG_*`, ...). It is a noise DENYLIST: an allowlist can never
//! enumerate `cargo:rustc-env` names, and dropping one hard-fails `env!()` in
//! the daemon-spawned rustc.
//!
//! The Phase 5b chunked-reply and Phase 5d request-encoding contracts that
//! used to live here went with the direct-IPC compile verb in soldr#2424.
//! Compile output now streams as SESSION frames, covered by `soldr-daemon`'s
//! `session_serve` tests.

#[test]
fn is_compile_env_var_drops_cargo_pkg_noise() {
    // The filter must keep cargo's compile contract vars (and, per
    // the linux-arm-musl crgx regression, any build-script-emitted
    // `cargo:rustc-env` var — arbitrary names, so the filter is a
    // noise denylist) while still dropping interactive-session
    // noise from the per-compile prost payload.
    use soldr_cli::compile_dispatch::is_compile_env_var;
    for kept in [
        "CARGO_PKG_NAME",
        "CARGO_PKG_VERSION",
        "CARGO_CFG_TARGET_ARCH",
        // build-script `cargo:rustc-env` vars — names are arbitrary
        "CRGX_TARGET",
        "VERGEN_GIT_SHA",
    ] {
        assert!(
            is_compile_env_var(kept),
            "{kept} must be forwarded (cargo compile contract / \
                 build-script rustc-env)"
        );
    }
    // Known session-noise vars must stay out of the payload.
    for dropped in [
        "PROMPT",
        "PS1",
        "OLDPWD",
        "SHLVL",
        "DISPLAY",
        "GDM_LANG",
        "DBUS_SESSION_BUS_ADDRESS",
        "XDG_SESSION_TYPE",
    ] {
        assert!(
            !is_compile_env_var(dropped),
            "{dropped} must be dropped — Phase 5c env filter (#981) keeps the per-compile \
                 prost payload under ~5 KB; if this fails the daemon's tokio runtime burns \
                 cycles encoding noise"
        );
    }
}
