//! `soldr env --target <triple-or-alias>` — print the complete blessed
//! cross-compile environment in shell-eval / shell-export / JSON form.
//! soldr#938; unified with `soldr prepare --github-env` in soldr#2304.
//!
//! One source of truth: this verb runs the same target preparation as
//! `soldr prepare` / `soldr build` (materializing the toolchain as
//! needed) and projects `prepare_github_env::exported_env_pairs` — the
//! exact `(key, value)` list the GitHub path writes to `$GITHUB_ENV`.
//! Before soldr#2304, this verb computed a second, divergent env (a
//! hardcoded linker guess that could contradict the blessed prep); the
//! divergent path is gone.
//!
//! Usage shapes:
//!
//! ```text
//! eval "$(soldr env --target mac-arm64)"
//! soldr env --target win-x64 --shell-export
//! soldr env --target linux-x64-musl --json
//! ```

use crate::core::{SoldrError, SoldrPaths};
use crate::target_alias::{resolve_soldr_target, AliasError};

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
    plan_only: bool,
) -> Result<i32, SoldrError> {
    let resolved = resolve_soldr_target(target_input).map_err(map_alias_err)?;

    // `--plan-only` (requires --json): resolution/introspection without
    // materializing anything — the alias-parity tests and IDE tooling
    // probe all sixteen canonical inputs this way, which must not cost
    // eight toolchain downloads. `env` is null so the payload can never
    // be mistaken for the prepared environment.
    if plan_only {
        let target_plan = crate::target_lifecycle::plan(&resolved.rust_triple)?;
        let payload = serde_json::json!({
            "schema_version": 1,
            "command": "env",
            "input": resolved.input,
            "rust_triple": resolved.rust_triple,
            "via_alias": resolved.via_alias,
            "env": serde_json::Value::Null,
            "target_plan": target_plan,
        });
        println!("{}", serde_json::to_string(&payload).unwrap_or_default());
        return Ok(0);
    }

    // soldr#2554 contract: machine-readable mode suppresses every
    // unsolicited diagnostic — including the fetch/installer progress
    // the preparation below may emit. The marker is process-internal;
    // eval callers of the shell formats keep normal progress on stderr.
    if json {
        std::env::set_var(crate::core::quiet::QUIET_DIAGNOSTICS_ENV_VAR, "1");
    }
    let paths = SoldrPaths::new()?;
    let prep = crate::target_lifecycle::prepare_target(&paths, &resolved.rust_triple).await?;
    let pairs = crate::prepare_github_env::exported_env_pairs(&prep, &resolved.rust_triple)?;

    if json {
        let target_plan = crate::target_lifecycle::plan(&resolved.rust_triple)?;
        let env: serde_json::Map<String, serde_json::Value> = pairs
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect();
        let payload = serde_json::json!({
            "schema_version": 1,
            "command": "env",
            "input": resolved.input,
            "rust_triple": resolved.rust_triple,
            "via_alias": resolved.via_alias,
            "env": env,
            "target_plan": target_plan,
        });
        println!("{}", serde_json::to_string(&payload).unwrap_or_default());
    } else if shell_export {
        for (k, v) in &pairs {
            println!("export {k}={}", shell_quote(v));
        }
    } else {
        for (k, v) in &pairs {
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

    #[test]
    fn shell_quote_strips_bare_alnum() {
        assert_eq!(shell_quote("plain123"), "plain123");
        assert_eq!(shell_quote("/usr/local/bin"), "/usr/local/bin");
        // Anything with a space or special character gets wrapped.
        assert_eq!(shell_quote("with space"), "'with space'");
        assert_eq!(shell_quote("don't"), "'don'\\''t'");
    }
}
