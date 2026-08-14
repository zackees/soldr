//! Placement of Cargo config overrides around nested cargo subcommands.

use super::{first_cargo_subcommand_index, first_nextest_verb_index};

/// nextest verbs that build the workspace and therefore accept Cargo's
/// `--config` passthrough. Verified against `cargo nextest <verb> --help`,
/// which lists `--config <KEY=VALUE>` separately from its own `--config-file`.
/// Non-build verbs (`show-config`, `self`) take no Cargo options, so they keep
/// the pre-`nextest` placement and would surface an error if one were injected
/// — which is the honest outcome, since the override could not apply anyway.
const NEXTEST_BUILD_VERBS: &[&str] = &["run", "list", "archive"];

pub(super) fn insert_cargo_global_args(args: &[String], cargo_args: &[String]) -> Vec<String> {
    if cargo_args.is_empty() {
        return args.to_vec();
    }
    let mut out = args.to_vec();
    let cargo_subcommand = first_cargo_subcommand_index(args);
    // cargo-nextest owns the argument parser after Cargo dispatches the
    // `nextest` subcommand. Its build commands accept Cargo `--config`
    // overrides, but only after the inner verb. Injecting before `nextest`
    // makes nextest's top-level parser reject `--config` as a misspelling of
    // its unrelated `--config-file` option (soldr#2037).
    //
    // soldr#2493 widened this from `archive` alone to every build verb.
    // Retiring the per-test watchdog macro made `nextest run` the way the
    // suite is executed in CI, and `run` hit exactly the #2037 failure the
    // `archive` case was already fixed for:
    // `error: unexpected argument '--config' found`.
    let insert_at = cargo_subcommand
        .filter(|&index| args.get(index).is_some_and(|arg| arg == "nextest"))
        .and_then(|index| first_nextest_verb_index(args, index))
        .filter(|&index| {
            args.get(index)
                .is_some_and(|arg| NEXTEST_BUILD_VERBS.contains(&arg.as_str()))
        })
        .map(|index| index + 1)
        .or(cargo_subcommand)
        .unwrap_or(0);
    out.splice(insert_at..insert_at, cargo_args.iter().cloned());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argvec(args: &str) -> Vec<String> {
        args.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn inserts_after_nextest_archive_subcommand() {
        let args = argvec(
            "--manifest-path Cargo.toml nextest --color never archive --target x86_64-apple-darwin",
        );
        let cargo_args = vec![
            "--config".to_string(),
            "target.x86_64-apple-darwin.mimalloc.rustc-link-lib=[\"static=mimalloc\"]".to_string(),
        ];

        let got = insert_cargo_global_args(&args, &cargo_args);

        assert_eq!(
            got,
            argvec("--manifest-path Cargo.toml nextest --color never archive --config target.x86_64-apple-darwin.mimalloc.rustc-link-lib=[\"static=mimalloc\"] --target x86_64-apple-darwin")
        );
    }

    // soldr#2493 RED case: this is the exact invocation `_build-and-test.yml`
    // runs now that the suite executes under nextest, and it failed CI with
    // `error: unexpected argument '--config' found` before `run` joined the
    // build-verb list.
    #[test]
    fn inserts_after_nextest_run_subcommand() {
        let args =
            argvec("nextest run --workspace --lib --tests --target x86_64-unknown-linux-gnu");
        let cargo_args = vec![
            "--config".to_string(),
            "build.rustc-wrapper=\"soldr\"".to_string(),
        ];

        let got = insert_cargo_global_args(&args, &cargo_args);

        assert_eq!(
            got,
            argvec(
                "nextest run --config build.rustc-wrapper=\"soldr\" --workspace --lib --tests --target x86_64-unknown-linux-gnu"
            )
        );
    }

    #[test]
    fn inserts_after_nextest_list_subcommand() {
        let args = argvec("nextest list --workspace");
        let cargo_args = vec!["--config".to_string(), "build.jobs=2".to_string()];

        let got = insert_cargo_global_args(&args, &cargo_args);

        assert_eq!(
            got,
            argvec("nextest list --config build.jobs=2 --workspace")
        );
    }

    #[test]
    fn stays_before_regular_cargo_subcommand() {
        let args = argvec("--manifest-path Cargo.toml build --target x86_64-apple-darwin");
        let cargo_args = vec![
            "--config".to_string(),
            "target.x86_64-apple-darwin.mimalloc.rustc-link-lib=[\"static=mimalloc\"]".to_string(),
        ];

        let got = insert_cargo_global_args(&args, &cargo_args);

        assert_eq!(
            got,
            argvec("--manifest-path Cargo.toml --config target.x86_64-apple-darwin.mimalloc.rustc-link-lib=[\"static=mimalloc\"] build --target x86_64-apple-darwin")
        );
    }
}
