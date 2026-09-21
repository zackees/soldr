//! Target-triple resolution and `SOLDR_LINKER` injection on the cargo
//! subprocess command line.
//!
//! Lifted out of the front-door entry point so the policy can be unit-
//! tested in isolation and re-used by other dispatch paths in the
//! future without dragging the full `run_cargo_front_door` body along.

use crate::core::{SoldrError, SoldrPaths};
use crate::{linker, LINKER_ENV_VAR};

use super::subcommand::{cargo_args_specify_target, cargo_args_target_value};

/// Whether soldr should inject its Windows MSVC build target, given the two
/// caller-observable inputs plus whether `CARGO_BUILD_TARGET` is already set.
/// Pure (no env / no `cfg!`) so the policy is deterministically unit-testable on
/// any host.
///
/// soldr#2350: a `cargo dylint` lint library is a HOST cdylib, loaded in-process
/// by the driver and never cross-compiled. Injecting the MSVC build target nests
/// the `dylint-link`-stamped `@<toolchain>.dll` under
/// `target/.../<triple>/release/`, but cargo-dylint's library lookup expects
/// `target/.../release/` -> "Could not find ... despite successful build". A
/// host-default build lands it where cargo-dylint looks. cc-rs is unaffected:
/// the explicit target only matters for *cross* C caching (see
/// `known_cargo_build_target` below), and dylint builds are host builds.
fn should_inject_windows_target_inner(
    args: &[String],
    dylint_requested: bool,
    build_target_env_set: bool,
) -> bool {
    if dylint_requested {
        return false;
    }
    !(cargo_args_specify_target(args) || build_target_env_set)
}

/// Env-reading wrapper over [`should_inject_windows_target_inner`].
pub(super) fn should_inject_windows_target(args: &[String], dylint_requested: bool) -> bool {
    should_inject_windows_target_inner(
        args,
        dylint_requested,
        std::env::var_os("CARGO_BUILD_TARGET").is_some(),
    )
}

pub(super) fn default_cargo_build_target(
    args: &[String],
    dylint_requested: bool,
) -> Result<Option<String>, SoldrError> {
    if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Windows
        || !should_inject_windows_target(args, dylint_requested)
    {
        return Ok(None);
    }

    Ok(Some(crate::core::TargetTriple::detect()?.triple()))
}

/// Return the target Cargo will build when soldr can know it without
/// falling back to host auto-detection.
///
/// `default_cargo_build_target` has a narrower job: inject Windows' native
/// MSVC default only when the user did not pass a target. Native C caching
/// needs the explicit target too, otherwise cross builds only get a generic
/// `CC` wrapper and cc-rs can fall back to the host compiler.
pub(super) fn known_cargo_build_target(
    args: &[String],
    defaulted_target: Option<&str>,
) -> Option<String> {
    let env_target = std::env::var_os("CARGO_BUILD_TARGET")
        .and_then(|target| target.to_str().map(str::to_string));
    known_cargo_build_target_inner(args, defaulted_target, env_target.as_deref())
}

fn known_cargo_build_target_inner(
    args: &[String],
    defaulted_target: Option<&str>,
    env_target: Option<&str>,
) -> Option<String> {
    // Cargo's explicit --target always outranks CARGO_BUILD_TARGET.
    if let Some(target) = cargo_args_target_value(args) {
        return Some(target);
    }
    if let Some(target) = defaulted_target {
        return Some(target.to_string());
    }
    if let Some(target) = env_target {
        let target = target.trim();
        if !target.is_empty() {
            return Some(target.to_string());
        }
    }
    None
}

/// Apply the `SOLDR_LINKER` / `config.toml linker = ...` override (issue
/// #285) to the cargo subprocess command.
///
/// The active target triple is resolved in the same order as cargo:
/// 1. an explicit `CARGO_BUILD_TARGET` injected by `default_cargo_build_target`,
/// 2. a `CARGO_BUILD_TARGET` already in the parent env,
/// 3. an `--target` flag inside `args`,
/// 4. the auto-detected host triple from `TargetTriple::detect()`.
pub(super) async fn apply_linker_override(
    command: &mut std::process::Command,
    args: &[String],
    explicit_target: Option<&str>,
    paths: &SoldrPaths,
) -> Result<(), SoldrError> {
    let config = paths
        .load_config()
        .map_err(|error| SoldrError::Other(error.to_string()))?;
    let linker_env = std::env::var_os(LINKER_ENV_VAR);
    let choice = linker::from_env_and_config(linker_env.as_deref(), config.linker.as_deref())?;
    let automatic = linker_env.is_none() && config.linker.is_none();
    if matches!(choice, linker::LinkerChoice::Default) {
        // Fast-path: skip target detection entirely when there is nothing
        // to inject. Keeps `soldr cargo` no-ops on platforms where target
        // detection might fail or be slow.
        return Ok(());
    }

    // `Fast` is the automatic/default linker (soldr#3262): it is a best-effort
    // convenience, not a hard requirement. If the host triple cannot be
    // detected (e.g. a repo-local fake rustc without rustup, or a non-build
    // command like `cargo --version`), skip injection rather than failing the
    // whole cargo invocation. An explicit `reld`/`mold`/`rust-lld` request
    // still surfaces the detection error.
    let target = match resolve_active_target_triple(args, explicit_target) {
        Ok(target) => target,
        Err(_) if matches!(choice, linker::LinkerChoice::Fast) => return Ok(()),
        Err(error) => return Err(error),
    };

    // soldr#3277: Cargo gives CARGO_TARGET_<TRIPLE>_LINKER/_RUSTFLAGS env vars
    // precedence over `.cargo/config.toml`, so injecting the *automatic*
    // default here would silently replace a linker the project declared for
    // this target. `apply_pep517_override` guards the same thing; both call
    // sites now share one predicate instead of growing a third copy. An
    // explicit SOLDR_LINKER / config.toml `linker =` is a user decision and
    // still wins.
    if automatic && linker::project_declares_target_linker(&target) {
        return Ok(());
    }

    let mut injection = linker::resolve_for_target(choice, &target)?;
    if matches!(choice, linker::LinkerChoice::Reld) {
        let reld = crate::fetch::ensure_reld(paths).await?;
        inject_resolved_reld(&mut injection, &reld)?;
    }
    let prefix = linker::cargo_target_env_prefix(&target);
    let linker_key = format!("CARGO_TARGET_{prefix}_LINKER");
    let rustflags_key = format!("CARGO_TARGET_{prefix}_RUSTFLAGS");
    // A target-scoped linker/rustflags value is more specific than soldr's
    // convenience linker selection. This matters for cross builds: the
    // workflow may already have installed a target-aware Zig wrapper while
    // SOLDR_LINKER=fast is inherited from setup-soldr. Do not replace an
    // explicit target toolchain with the host clang/LLD fallback.
    if let Some(linker_path) = injection.linker {
        if !linker::effective_command_env_is_non_empty(command, &linker_key) {
            command.env(&linker_key, linker_path);
        }
    }
    if let Some(rustflags) = injection.rustflags {
        if !linker::effective_command_env_is_non_empty(command, &rustflags_key) {
            command.env(&rustflags_key, rustflags);
        }
    }
    Ok(())
}

/// Replace the bare `reld` token in an explicit linker injection with the
/// verified host executable resolved before Cargo starts.
fn inject_resolved_reld(
    injection: &mut linker::LinkerInjection,
    reld: &std::path::Path,
) -> Result<(), SoldrError> {
    let reld = reld.to_str().ok_or_else(|| {
        SoldrError::Other(format!(
            "managed reld path is not valid UTF-8: {}",
            reld.display()
        ))
    })?;
    let mut replaced = false;
    if injection.linker.as_deref() == Some("reld") {
        injection.linker = Some(reld.to_string());
        replaced = true;
    }
    if let Some(flags) = injection.rustflags.as_mut() {
        if flags.contains("--ld-path=reld") {
            *flags = flags.replacen("--ld-path=reld", &format!("--ld-path={reld}"), 1);
            replaced = true;
        }
    }
    if replaced {
        Ok(())
    } else {
        Err(SoldrError::Other(
            "explicit reld linker selection produced no reld injection".to_string(),
        ))
    }
}

fn resolve_active_target_triple(
    args: &[String],
    explicit_target: Option<&str>,
) -> Result<String, SoldrError> {
    let env_target = std::env::var_os("CARGO_BUILD_TARGET")
        .and_then(|target| target.to_str().map(str::to_string));
    if let Some(target) =
        known_cargo_build_target_inner(args, explicit_target, env_target.as_deref())
    {
        return Ok(target);
    }
    Ok(crate::core::TargetTriple::detect()?.triple())
}

#[cfg(test)]
mod tests {
    use super::{
        inject_resolved_reld, known_cargo_build_target_inner as known_target,
        should_inject_windows_target_inner as inject,
    };
    use crate::linker::LinkerInjection;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn dylint_never_injects_windows_target() {
        // soldr#2350: dylint libs are host cdylibs; injecting the MSVC target
        // nests the stamped DLL where cargo-dylint can't find it. Suppressed
        // regardless of an explicit --target or a set CARGO_BUILD_TARGET.
        assert!(!inject(&args("dylint --all"), true, false));
        assert!(!inject(&args("dylint"), true, true));
    }

    #[test]
    fn ordinary_build_injects_when_target_unspecified() {
        // The MSVC-on-Windows default still applies to ordinary builds.
        assert!(inject(&args("build --release"), false, false));
        assert!(inject(&args("test"), false, false));
    }

    #[test]
    fn explicit_target_or_env_suppresses_injection() {
        // Unchanged: an explicit --target or a caller-set CARGO_BUILD_TARGET
        // means soldr does not inject its own default.
        assert!(!inject(
            &args("build --target x86_64-pc-windows-gnu"),
            false,
            false,
        ));
        assert!(!inject(&args("build"), false, true));
    }

    #[test]
    fn explicit_target_beats_environment_target() {
        assert_eq!(
            known_target(
                &args("build --target x86_64-pc-windows-msvc"),
                None,
                Some("x86_64-unknown-linux-gnu"),
            ),
            Some("x86_64-pc-windows-msvc".to_string()),
        );
    }

    #[test]
    fn resolved_reld_replaces_bare_linux_ld_path() {
        let mut injection = LinkerInjection {
            linker: Some("clang".to_string()),
            rustflags: Some("-C link-arg=--ld-path=reld".to_string()),
        };
        inject_resolved_reld(&mut injection, std::path::Path::new("/managed/reld"))
            .expect("replace reld");
        assert_eq!(injection.linker.as_deref(), Some("clang"));
        assert_eq!(
            injection.rustflags.as_deref(),
            Some("-C link-arg=--ld-path=/managed/reld")
        );
    }

    #[test]
    fn resolved_reld_replaces_direct_linker_path() {
        let mut injection = LinkerInjection {
            linker: Some("reld".to_string()),
            rustflags: None,
        };
        inject_resolved_reld(&mut injection, std::path::Path::new("C:/managed/reld.exe"))
            .expect("replace reld");
        assert_eq!(injection.linker.as_deref(), Some("C:/managed/reld.exe"));
    }
}
