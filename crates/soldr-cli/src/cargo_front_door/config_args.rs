//! Placement of Cargo config overrides around nested cargo subcommands.

use super::{first_cargo_subcommand_index, first_nextest_verb_index};

pub(super) fn insert_cargo_global_args(args: &[String], cargo_args: &[String]) -> Vec<String> {
    if cargo_args.is_empty() {
        return args.to_vec();
    }
    let mut out = args.to_vec();
    let cargo_subcommand = first_cargo_subcommand_index(args);
    // cargo-nextest owns the argument parser after Cargo dispatches the
    // `nextest` subcommand. Its build commands accept Cargo `--config`
    // overrides, but only after the inner command (`archive` here). Injecting
    // before `nextest` makes nextest's top-level parser reject `--config` as a
    // misspelling of its unrelated `--config-file` option (soldr#2037).
    let insert_at = cargo_subcommand
        .filter(|&index| args.get(index).is_some_and(|arg| arg == "nextest"))
        .and_then(|index| first_nextest_verb_index(args, index))
        .filter(|&index| args.get(index).is_some_and(|arg| arg == "archive"))
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
