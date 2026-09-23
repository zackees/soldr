use crate::ci_test::model::{Invocation, OutputFormat, Scope};
use crate::core::SoldrError;

pub(crate) fn parse(args: &[String]) -> Result<Invocation, SoldrError> {
    let mut explain = false;
    let mut format = OutputFormat::Human;
    let mut scope = Scope::default();
    let mut requested_target = None;
    let mut no_run = false;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        let next = |flag: &str, index: &mut usize| -> Result<String, SoldrError> {
            *index += 1;
            args.get(*index)
                .cloned()
                .ok_or_else(|| SoldrError::Other(format!("soldr ci-test: {flag} requires a value")))
        };
        match arg {
            "--explain-plan" => explain = true,
            "--no-run" => no_run = true,
            "--format" => {
                let value = next("--format", &mut index)?;
                format = parse_format(&value)?;
            }
            value if value.starts_with("--format=") => format = parse_format(&value[9..])?,
            "--package" | "-p" => scope.packages.push(next("--package", &mut index)?),
            value if value.starts_with("--package=") => scope.packages.push(value[10..].into()),
            "--features" => add_features(&mut scope.features, &next("--features", &mut index)?),
            value if value.starts_with("--features=") => {
                add_features(&mut scope.features, &value[11..])
            }
            "--all-features" => scope.all_features = true,
            "--no-default-features" => scope.no_default_features = true,
            "--all-targets" | "--workspace" => {
                // Canonical, fixed workspace scope; accepting these harmless
                // spellings makes a copied CI command explain the same plan.
            }
            "--target" => set_target(&mut requested_target, next("--target", &mut index)?)?,
            value if value.starts_with("--target=") => {
                set_target(&mut requested_target, value[9..].into())?
            }
            "--target-dir" | "--profile" | "--toolchain" | "--manifest-path" => {
                return Err(incompatible_override(arg));
            }
            value
                if value.starts_with("--target-dir=")
                    || value.starts_with("--profile=")
                    || value.starts_with("--toolchain=")
                    || value.starts_with("--manifest-path=")
                    || value.starts_with('+')
                    || value == "--release" =>
            {
                return Err(incompatible_override(arg))
            }
            "--" => {
                return Err(SoldrError::Other(
                    "soldr ci-test: compiler arguments after `--` are incompatible with the frozen validation plan".into(),
                ));
            }
            _ => {
                return Err(SoldrError::Other(format!(
                    "soldr ci-test: unsupported option {arg:?}; supported options include --target, --no-run, --package/-p, --features, --all-features, and --no-default-features"
                )));
            }
        }
        index += 1;
    }
    if !explain && !matches!(format, OutputFormat::Human) {
        return Err(SoldrError::Other(
            "soldr ci-test: --format is only valid with --explain-plan".into(),
        ));
    }
    if scope.all_features && scope.no_default_features {
        return Err(SoldrError::Other(
            "soldr ci-test: --all-features conflicts with --no-default-features".into(),
        ));
    }
    scope.packages.sort();
    scope.packages.dedup();
    scope.features.sort();
    scope.features.dedup();
    Ok(Invocation {
        explain,
        format,
        scope,
        requested_target,
        no_run,
    })
}

fn parse_format(value: &str) -> Result<OutputFormat, SoldrError> {
    OutputFormat::parse(value).ok_or_else(|| {
        SoldrError::Other(format!(
            "soldr ci-test: unknown --format {value:?}; expected human or json"
        ))
    })
}

fn set_target(slot: &mut Option<String>, value: String) -> Result<(), SoldrError> {
    if slot.is_some() {
        return Err(SoldrError::Other(
            "soldr ci-test: only one --target is allowed per invocation".into(),
        ));
    }
    if value.is_empty() {
        return Err(SoldrError::Other(
            "soldr ci-test: --target requires a non-empty triple".into(),
        ));
    }
    let mut args = vec!["--target".into(), value];
    crate::target_alias::normalize_target_aliases_in_args(&mut args);
    let target = args.pop().expect("target argument exists");
    if !target
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(SoldrError::Other(format!(
            "soldr ci-test: --target {target:?} must be a Rust target triple, not a path"
        )));
    }
    *slot = Some(target);
    Ok(())
}

fn incompatible_override(option: &str) -> SoldrError {
    SoldrError::Other(format!(
        "soldr ci-test: {option} is incompatible with the frozen validation domain; use `soldr cargo ...` for an explicit toolchain, profile, target-dir, or manifest override"
    ))
}

fn add_features(features: &mut Vec<String>, value: &str) {
    features.extend(
        value
            .split([',', ' '])
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).into()).collect()
    }

    #[test]
    fn accepts_scope_flags() {
        let parsed = parse(&strings(&["-p", "soldr-cli", "--features", "a,b"])).unwrap();
        assert_eq!(parsed.scope.packages, ["soldr-cli"]);
        assert_eq!(parsed.scope.features, ["a", "b"]);
    }

    #[test]
    fn preserves_target_for_validation() {
        let parsed = parse(&strings(&["--target=x86_64-unknown-linux-gnu"])).unwrap();
        assert_eq!(
            parsed.requested_target.as_deref(),
            Some("x86_64-unknown-linux-gnu")
        );
        for override_arg in ["+nightly", "--release"] {
            let error = parse(&strings(&[override_arg])).unwrap_err();
            assert!(error.to_string().contains("frozen validation domain"));
        }
    }

    #[test]
    fn compile_only_accepts_one_normalized_target() {
        let parsed = parse(&strings(&["--no-run", "--target", "mac-arm64"])).unwrap();
        assert!(parsed.no_run);
        assert_eq!(
            parsed.requested_target.as_deref(),
            Some("aarch64-apple-darwin")
        );
        let error = parse(&strings(&["--target", "mac-arm64", "--target", "win-x64"])).unwrap_err();
        assert!(error.to_string().contains("only one --target"));
        let error = parse(&strings(&["--no-run", "--target", "../archive"])).unwrap_err();
        assert!(error.to_string().contains("must be a Rust target triple"));
    }
}
