use crate::ci_test::model::{Invocation, OutputFormat, Scope};
use crate::core::SoldrError;

pub(crate) fn parse(args: &[String]) -> Result<Invocation, SoldrError> {
    let mut explain = false;
    let mut format = OutputFormat::Human;
    let mut scope = Scope::default();
    let mut requested_target = None;
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
                // Canonical, fixed host scope; accepting these harmless
                // spellings makes a copied CI command explain the same plan.
            }
            "--target" => requested_target = Some(next("--target", &mut index)?),
            value if value.starts_with("--target=") => requested_target = Some(value[9..].into()),
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
                    "soldr ci-test: compiler arguments after `--` are incompatible with the frozen host-validation plan".into(),
                ));
            }
            _ => {
                return Err(SoldrError::Other(format!(
                    "soldr ci-test: unsupported scope option {arg:?}; supported options are --package/-p, --features, --all-features, and --no-default-features"
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
    })
}

fn parse_format(value: &str) -> Result<OutputFormat, SoldrError> {
    OutputFormat::parse(value).ok_or_else(|| {
        SoldrError::Other(format!(
            "soldr ci-test: unknown --format {value:?}; expected human or json"
        ))
    })
}

fn incompatible_override(option: &str) -> SoldrError {
    SoldrError::Other(format!(
        "soldr ci-test: {option} is incompatible with the frozen host-validation domain; use `soldr cargo ...` for an explicit target, toolchain, profile, target-dir, or manifest override"
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
    fn accepts_only_host_scope_flags() {
        let parsed = parse(&strings(&["-p", "soldr-cli", "--features", "a,b"])).unwrap();
        assert_eq!(parsed.scope.packages, ["soldr-cli"]);
        assert_eq!(parsed.scope.features, ["a", "b"]);
    }

    #[test]
    fn preserves_target_for_host_validation() {
        let parsed = parse(&strings(&["--target=x86_64-unknown-linux-gnu"])).unwrap();
        assert_eq!(
            parsed.requested_target.as_deref(),
            Some("x86_64-unknown-linux-gnu")
        );
        for override_arg in ["+nightly", "--release"] {
            let error = parse(&strings(&[override_arg])).unwrap_err();
            assert!(error.to_string().contains("frozen host-validation domain"));
        }
    }
}
