//! `soldr env --target <triple-or-alias>` — print the cross-compile
//! env block in shell-eval / shell-export / JSON form. soldr#938.
//!
//! The target-derived block matches the OS SDK and linker settings used by
//! soldr's cargo front door / `soldr prepare`. Python ABI configuration is
//! resolved separately from workspace metadata: ordinary cross-builds do not
//! receive blanket `PYO3_CROSS_*` values.
//!
//! Usage shapes:
//!
//! ```text
//! eval "$(soldr env --target mac-arm64)"
//! soldr env --target win-x64 --shell-export
//! soldr env --target linux-x64-musl --json
//! ```
//!
//! `--json` also reports the shared PyO3 build plan. Target Python assets are
//! materialized only when the caller explicitly selects compatibility mode;
//! they are not part of target OS SDK preparation.

use std::collections::BTreeMap;

use crate::core::{SoldrError, SoldrPaths};
use crate::target_alias::{resolve_soldr_target, AliasError};

/// Returns the env block soldr would set when cross-compiling to
/// `target`. The block is target-derived but does NOT touch disk —
/// `soldr prepare` still owns the actual asset fetching.
pub fn build_env_block(rust_triple: &str) -> Result<BTreeMap<String, String>, SoldrError> {
    build_env_plan(rust_triple).map(|(env, _)| env)
}

/// [`build_env_block`] against an explicit workspace root, so a caller can
/// decide what the PyO3 detection sees instead of inheriting the ambient cwd.
#[cfg(test)]
fn build_env_block_in(
    workspace_root: &std::path::Path,
    rust_triple: &str,
) -> Result<BTreeMap<String, String>, SoldrError> {
    build_env_plan_in(workspace_root, rust_triple).map(|(env, _)| env)
}

fn build_env_plan(
    rust_triple: &str,
) -> Result<(BTreeMap<String, String>, crate::pyo3_detect::Pyo3BuildPlan), SoldrError> {
    let workspace_root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    build_env_plan_in(&workspace_root, rust_triple)
}

/// [`build_env_plan`] against an explicit workspace root.
///
/// The PyO3 half of the block is decided by workspace metadata, so a caller
/// that does not pin the root inherits whatever cwd it happens to run in.
/// That is fine in production, where cwd *is* the workspace, and wrong in a
/// test, which would otherwise assert against the machine it lands on.
fn build_env_plan_in(
    workspace_root: &std::path::Path,
    rust_triple: &str,
) -> Result<(BTreeMap<String, String>, crate::pyo3_detect::Pyo3BuildPlan), SoldrError> {
    let mut env = BTreeMap::new();

    // SDKROOT for Apple targets. Uses the managed Apple SDK pin —
    // the URL-substring picker (soldr#996) will pick the right
    // catalogue row when SOLDR_APPLE_SDK_SHAPE / _VERSION are set.
    if rust_triple.ends_with("-apple-darwin") {
        let paths = SoldrPaths::new()?;
        let sdk_dir = crate::fetch::apple_sdk::sdk_dir_for_target(&paths, Some(rust_triple));
        env.insert("SDKROOT".to_string(), sdk_dir.display().to_string());
    }

    // Linker selection — clang via lld. The actual clang path is
    // tied to soldr's bundled LLVM (soldr#934). When that catalogue
    // row ships the CARGO_TARGET_*_LINKER variable should resolve
    // through the SoldrPaths::bin/llvm-tools location.
    let triple_upper = rust_triple.to_ascii_uppercase().replace('-', "_");
    env.insert(
        format!("CARGO_TARGET_{triple_upper}_LINKER"),
        "clang".to_string(),
    );

    // Python ABI policy is separate from the target SDK/linker block.
    // The shared resolver only emits PYO3_NO_PYTHON when workspace
    // metadata proves this is an ABI3 extension cross-build.
    let plan = crate::pyo3_detect::resolve_for_invocation(workspace_root, &[], Some(rust_triple));
    env.extend(plan.env.clone());

    Ok((env, plan))
}

/// Run the `env` subcommand. Three output forms:
///
/// * Default (`--target X`): bare `KEY=VALUE` lines (sourceable via
///   `set -a` in shell or piped through `eval`).
/// * `--shell-export`: `export KEY=VALUE` lines.
/// * `--json`: stable JSON `{ schema_version: 1, target: …, env: { … } }`.
pub async fn run_env_command(
    target_input: &str,
    shell_export: bool,
    json: bool,
) -> Result<i32, SoldrError> {
    let resolved = resolve_soldr_target(target_input).map_err(map_alias_err)?;

    let (mut env, mut pyo3_plan) = build_env_plan(&resolved.rust_triple)?;
    if pyo3_plan.needs_python_sysroot {
        let paths = SoldrPaths::new()?;
        pyo3_plan.materialize_compatibility(&paths).await?;
        env.extend(pyo3_plan.env.clone());
    }

    if json {
        let payload = serde_json::json!({
            "schema_version": 1,
            "input": resolved.input,
            "rust_triple": resolved.rust_triple,
            "via_alias": resolved.via_alias,
            "env": env,
            "pyo3_plan": pyo3_plan,
        });
        println!("{}", serde_json::to_string(&payload).unwrap_or_default());
    } else if shell_export {
        for (k, v) in &env {
            println!("export {k}={}", shell_quote(v));
        }
    } else {
        for (k, v) in &env {
            println!("{k}={}", shell_quote(v));
        }
    }
    Ok(0)
}

fn map_alias_err(err: AliasError) -> SoldrError {
    SoldrError::Other(err.to_string())
}

/// Quote a value for safe `eval` consumption — wrap in single
/// quotes, escape any embedded single-quotes. Same convention `bash`
/// uses in its `$'…'` form.
fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '/' || c == '_' || c == '-' || c == '.')
    {
        // Bare value is safe to emit unquoted.
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // soldr#1663 / release 0.8.26: these tests READ process-global state
    // rather than mutate it -- `build_env_block` calls
    // `pyo3_detect::caller_pyo3_env()`, which collects every ambient `PYO3_*`
    // variable. A reader with no barrier races every test that sets one, and
    // `env_block_does_not_guess_pyo3_no_python` duly failed on macOS with
    // PYO3_NO_PYTHON present. Take the same crate-wide barrier the mutators
    // take; the env-lock lint only tracks mutation sites, so it cannot catch
    // an unguarded reader for us.
    use crate::TEST_PROCESS_ENV_LOCK as ENV_LOCK;

    crate::timed_test!(env_block_darwin_carries_sdkroot, {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let env = build_env_block("aarch64-apple-darwin").expect("ok");
        assert!(env.contains_key("SDKROOT"), "darwin must export SDKROOT");
        assert!(env.contains_key("CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER"));
    });

    crate::timed_test!(env_block_windows_lacks_sdkroot, {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let env = build_env_block("x86_64-pc-windows-msvc").expect("ok");
        assert!(!env.contains_key("SDKROOT"));
        assert!(env.contains_key("CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER"));
    });

    crate::timed_test!(env_block_does_not_guess_pyo3_no_python, {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Resolve against an empty workspace rather than the ambient cwd.
        // PYO3_NO_PYTHON is emitted when workspace metadata proves an ABI3
        // extension cross-build, so a test that does not pin the root is
        // asserting a negative about an input it does not control -- it
        // passed or failed depending on which runner it landed on.
        let empty = tempfile::tempdir().expect("tempdir");
        for triple in [
            "x86_64-pc-windows-msvc",
            "aarch64-apple-darwin",
            "x86_64-unknown-linux-musl",
        ] {
            let env = build_env_block_in(empty.path(), triple).expect("ok");
            assert!(
                !env.contains_key("PYO3_NO_PYTHON"),
                "no workspace metadata means no ABI3 proof, so the key must not                  be guessed for {triple}"
            );
        }
    });

    crate::timed_test!(shell_quote_strips_bare_alnum, {
        assert_eq!(shell_quote("plain123"), "plain123");
        assert_eq!(shell_quote("/usr/local/bin"), "/usr/local/bin");
        // Anything with a space or special character gets wrapped.
        assert_eq!(shell_quote("with space"), "'with space'");
        assert_eq!(shell_quote("don't"), "'don'\\''t'");
    });

    // soldr#1927: this began as a `probe_cwd_decides` diagnostic that chdir'd
    // into a fixture, printed whether PYO3_NO_PYTHON leaked, and asserted
    // nothing. It proved the mechanism -- the workspace root decides the
    // outcome -- but a test that cannot fail is not a guard, and worse, its
    // `set_current_dir` was itself the process-global hazard the issue
    // describes: `cargo test` runs these as threads in one process, so a chdir
    // is visible to every other test that reads the CWD.
    //
    // Keep the proof, drop the chdir. Resolving through the explicit-root seam
    // asserts the same causal claim without touching global state.
    crate::timed_test!(workspace_root_decides_pyo3_no_python, {
        // Still needs the barrier: the root is explicit now, but the plan also
        // consults `caller_pyo3_env()`, and a leaked ambient `PYO3_*` takes the
        // CallerConfigured early return -- which would suppress the key and
        // fail the first assertion below for a reason that has nothing to do
        // with the workspace root.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let abi3 = tempfile::tempdir().expect("tmp");
        std::fs::write(
            abi3.path().join("Cargo.toml"),
            "[package]
name = \"ext\"
version = \"0.1.0\"
edition = \"2021\"

[lib]
crate-type = [\"cdylib\"]

[dependencies]
pyo3 = { version = \"0.22\", features = [\"abi3-py38\", \"extension-module\"] }
",
        )
        .expect("write manifest");
        std::fs::create_dir_all(abi3.path().join("src")).expect("src");
        std::fs::write(abi3.path().join("src").join("lib.rs"), "").expect("lib");

        // The target has to be *cross* on every runner, not just on most of
        // them. `resolve_for_target` returns `PlanMode::Native` with an empty
        // env as soon as `host == target` (pyo3_detect.rs:161), before it can
        // reach the ABI3 branch that emits the key -- so hardcoding one triple
        // asserts a cross-only rule on whichever runner happens to be that
        // host. `aarch64-apple-darwin` is exactly that runner in this repo.
        let cross = if crate::pyo3_detect::host_triple() == "aarch64-apple-darwin" {
            "x86_64-unknown-linux-gnu"
        } else {
            "aarch64-apple-darwin"
        };
        assert_ne!(
            cross,
            crate::pyo3_detect::host_triple(),
            "the target must be cross for the ABI3 branch to be reachable at all"
        );

        let from_abi3 = build_env_block_in(abi3.path(), cross).expect("ok");
        let empty = tempfile::tempdir().expect("tmp");
        let from_empty = build_env_block_in(empty.path(), cross).expect("ok");

        assert!(
            from_abi3.contains_key("PYO3_NO_PYTHON"),
            "an ABI3 extension workspace is the proof that lets soldr set \
             PYO3_NO_PYTHON; if this stops holding, the emission rule changed"
        );
        assert!(
            !from_empty.contains_key("PYO3_NO_PYTHON"),
            "no workspace metadata means no ABI3 proof, so the key must not be \
             guessed -- these two roots differing is exactly why callers must \
             pass the root rather than inherit the ambient CWD"
        );
    });
}
