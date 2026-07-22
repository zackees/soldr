//! Cache-aware unified validation command (soldr#1721).
//!
//! The command deliberately keeps compiler-bearing work on Soldr's cargo
//! front door while spawning dependency-only checks as cache-disabled child
//! Soldr commands. That preserves the pinned toolchain and managed tool
//! resolution without compiler-cache startup for deny, audit, or machete.

use crate::cargo_front_door;
use crate::core::SoldrError;
use crate::{current_soldr_binary, ZccacheSourceArg};
use std::process::{Child, Command};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LintMode {
    Rust,
    Deps,
    All,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LintPlan {
    mode: LintMode,
    scope: Vec<String>,
}

impl LintPlan {
    fn parse(args: &[String]) -> Result<Self, SoldrError> {
        let (mode, scope) = match args.first().map(String::as_str) {
            None => (LintMode::Rust, Vec::new()),
            Some("rust") => (LintMode::Rust, args[1..].to_vec()),
            Some("deps") => (LintMode::Deps, args[1..].to_vec()),
            Some("all") => (LintMode::All, args[1..].to_vec()),
            Some(value) if value.starts_with('-') => (LintMode::Rust, args.to_vec()),
            Some(value) => {
                return Err(SoldrError::Other(format!(
                    "lint: unknown suite {value:?}; expected rust, deps, or all"
                )))
            }
        };

        if scope.iter().any(|arg| arg == "--") {
            return Err(SoldrError::Other(
                "lint: compiler arguments after -- are not supported; pass cargo scope flags before the suite".into(),
            ));
        }
        Ok(Self { mode, scope })
    }

    fn rust_steps(&self, all_features: bool) -> Result<Vec<Vec<String>>, SoldrError> {
        let mut compiler_scope = self.scope.clone();
        if all_features {
            add_all_features(&mut compiler_scope)?;
        }

        let mut fmt = vec!["fmt".into(), "--all".into()];
        fmt.extend(fmt_compatible_scope(&self.scope));
        fmt.extend(["--".into(), "--check".into()]);

        let mut clippy = vec![
            "clippy".into(),
            "--workspace".into(),
            "--all-targets".into(),
        ];
        clippy.extend(compiler_scope.iter().cloned());
        clippy.extend(["--".into(), "-D".into(), "warnings".into()]);

        let mut dylint = vec!["dylint".into(), "--all".into(), "--".into()];
        dylint.extend(["--workspace".into(), "--all-targets".into()]);
        dylint.extend(compiler_scope);

        Ok(vec![fmt, clippy, dylint])
    }

    fn dependency_steps(&self) -> Result<Vec<Vec<String>>, SoldrError> {
        let mut index = 0;
        while index < self.scope.len() {
            let arg = &self.scope[index];
            if arg == "--manifest-path" {
                if self.scope.get(index + 1).is_none() {
                    return Err(SoldrError::Other(
                        "lint deps: --manifest-path requires a path".into(),
                    ));
                }
                index += 2;
                continue;
            }
            if arg.starts_with("--manifest-path=") {
                index += 1;
                continue;
            }
            return Err(SoldrError::Other(
                "lint deps accepts only an optional --manifest-path scope; dependency tools do not share Cargo package/feature flags".into(),
            ));
        }
        Ok(vec![
            prepend("deny", &["check"], &self.scope),
            prepend("audit", &[], &self.scope),
            prepend("machete", &[], &self.scope),
        ])
    }

    fn exhaustive_steps(&self) -> Result<Vec<Vec<String>>, SoldrError> {
        let mut scope = self.scope.clone();
        add_all_features(&mut scope)?;
        let mut udeps = vec!["udeps".into(), "--workspace".into(), "--all-targets".into()];
        udeps.extend(scope);
        Ok(vec![udeps, vec!["semver-checks".into()]])
    }
}

fn prepend(subcommand: &str, fixed: &[&str], scope: &[String]) -> Vec<String> {
    let mut args = Vec::with_capacity(1 + fixed.len() + scope.len());
    args.extend(scope.iter().cloned());
    args.push(subcommand.into());
    args.extend(fixed.iter().map(|arg| (*arg).into()));
    args
}

fn fmt_compatible_scope(scope: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut index = 0;
    while index < scope.len() {
        let arg = &scope[index];
        if arg == "--manifest-path" || arg == "--package" || arg == "-p" {
            out.push(arg.clone());
            if let Some(value) = scope.get(index + 1) {
                out.push(value.clone());
                index += 1;
            }
        } else if arg.starts_with("--manifest-path=") || arg.starts_with("--package=") {
            out.push(arg.clone());
        }
        index += 1;
    }
    out
}

fn add_all_features(scope: &mut Vec<String>) -> Result<(), SoldrError> {
    if scope.iter().any(|arg| arg == "--no-default-features") {
        return Err(SoldrError::Other(
            "lint all always validates every feature and cannot be combined with --no-default-features".into(),
        ));
    }
    if !scope.iter().any(|arg| arg == "--all-features") {
        scope.push("--all-features".into());
    }
    Ok(())
}

pub(crate) async fn run_lint(
    args: &[String],
    cache_enabled: bool,
    zccache_source: ZccacheSourceArg,
    trust_inherited_soldr_env: bool,
) -> Result<i32, SoldrError> {
    let plan = LintPlan::parse(args)?;
    match plan.mode {
        LintMode::Rust => {
            run_compile_steps(
                plan.rust_steps(false)?,
                cache_enabled,
                zccache_source,
                trust_inherited_soldr_env,
            )
            .await
        }
        LintMode::Deps => run_dependency_steps(plan.dependency_steps()?, trust_inherited_soldr_env),
        LintMode::All => {
            let code = run_compile_steps(
                plan.rust_steps(true)?,
                cache_enabled,
                zccache_source,
                trust_inherited_soldr_env,
            )
            .await?;
            if code != 0 {
                return Ok(code);
            }
            let code = run_dependency_steps(plan.dependency_steps()?, trust_inherited_soldr_env)?;
            if code != 0 {
                return Ok(code);
            }
            run_compile_steps(
                plan.exhaustive_steps()?,
                cache_enabled,
                zccache_source,
                trust_inherited_soldr_env,
            )
            .await
        }
    }
}

async fn run_compile_steps(
    steps: Vec<Vec<String>>,
    cache_enabled: bool,
    zccache_source: ZccacheSourceArg,
    trust_inherited_soldr_env: bool,
) -> Result<i32, SoldrError> {
    for args in steps {
        let code = cargo_front_door::run_cargo_front_door(
            &args,
            cache_enabled,
            zccache_source,
            trust_inherited_soldr_env,
        )
        .await?;
        if code != 0 {
            return Ok(code);
        }
    }
    Ok(0)
}

fn run_dependency_steps(
    steps: Vec<Vec<String>>,
    trust_inherited_soldr_env: bool,
) -> Result<i32, SoldrError> {
    let soldr = current_soldr_binary()?;
    let mut children = Vec::with_capacity(steps.len());
    for args in steps {
        let mut command = Command::new(&soldr);
        command.arg("--no-cache");
        if trust_inherited_soldr_env {
            command.arg("--trust-inherited-soldr-env");
        }
        command.arg("cargo").args(args);
        cargo_front_door::configure_cargo_child_for_timeout(&mut command);
        children.push(command.spawn().map_err(|error| {
            SoldrError::Other(format!(
                "lint deps: failed to start child Soldr process: {error}"
            ))
        })?);
    }
    wait_for_parallel_children(&mut children)
}

fn wait_for_parallel_children(children: &mut [Child]) -> Result<i32, SoldrError> {
    loop {
        let mut complete = 0;
        for index in 0..children.len() {
            if let Some(status) = children[index].try_wait()? {
                complete += 1;
                if !status.success() {
                    let failed_id = children[index].id();
                    for other in children.iter_mut() {
                        if other.id() != failed_id {
                            let _ = cargo_front_door::kill_cargo_process_tree(other);
                            let _ = other.wait();
                        }
                    }
                    return Ok(status.code().unwrap_or(1));
                }
            }
        }
        if complete == children.len() {
            return Ok(0);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).into()).collect()
    }

    crate::timed_test!(default_suite_has_one_clippy_scope_without_check, {
        let plan = LintPlan::parse(&[]).unwrap();
        assert_eq!(plan.mode, LintMode::Rust);
        assert_eq!(
            plan.rust_steps(false).unwrap(),
            vec![
                strings(&["fmt", "--all", "--", "--check"]),
                strings(&[
                    "clippy",
                    "--workspace",
                    "--all-targets",
                    "--",
                    "-D",
                    "warnings"
                ]),
                strings(&["dylint", "--all", "--", "--workspace", "--all-targets"]),
            ]
        );
    });

    crate::timed_test!(all_suite_uses_all_features_for_compiler_steps, {
        let plan = LintPlan::parse(&strings(&["all", "--package", "soldr-cli"])).unwrap();
        let rust = plan.rust_steps(true).unwrap();
        assert!(rust[1].contains(&"--all-features".into()));
        assert!(rust[2].contains(&"--all-features".into()));
        let exhaustive = plan.exhaustive_steps().unwrap();
        assert_eq!(exhaustive[0][0], "udeps");
        assert!(exhaustive[0].contains(&"--all-features".into()));
        assert_eq!(exhaustive[1], strings(&["semver-checks"]));
    });

    crate::timed_test!(dependency_suite_is_limited_to_shared_manifest_scope, {
        let plan = LintPlan::parse(&strings(&["deps", "--manifest-path", "Cargo.toml"])).unwrap();
        assert_eq!(
            plan.dependency_steps().unwrap(),
            vec![
                strings(&["--manifest-path", "Cargo.toml", "deny", "check"]),
                strings(&["--manifest-path", "Cargo.toml", "audit"]),
                strings(&["--manifest-path", "Cargo.toml", "machete"]),
            ]
        );
        let invalid = LintPlan::parse(&strings(&["deps", "--all-features"])).unwrap();
        assert!(invalid.dependency_steps().is_err());
    });

    crate::timed_test!(unknown_suite_is_rejected, {
        let error = LintPlan::parse(&strings(&["everything"])).unwrap_err();
        assert!(error.to_string().contains("unknown suite"));
    });
}
