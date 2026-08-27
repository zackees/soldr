//! `soldr dylint prepare` — resolve and validate the complete Dylint
//! stack ahead of lint/test execution (soldr#2484 proposal item 5).
//!
//! The front door already auto-prepares on `soldr cargo dylint ...`;
//! this surface runs exactly that preparation pipeline standalone so a
//! caller (or CI step) can warm the stack deliberately, see what was
//! resolved, and know the next lint/test invocation will not stall on
//! downloads — pairing with the wrapper's fail-closed guard against
//! nested driver source builds.

use crate::core::{SoldrError, SoldrPaths};

pub(crate) async fn run(args: &[String]) -> Result<i32, SoldrError> {
    if !args.is_empty() {
        return Err(SoldrError::Other(format!(
            "soldr dylint prepare takes no arguments (got {args:?})"
        )));
    }
    let paths = SoldrPaths::new()?;
    paths.ensure_dirs()?;
    let workspace_root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

    eprintln!("soldr: dylint prepare: resolving cargo-dylint + dylint-link prebuilts");
    let bootstrap =
        crate::cargo_front_door::ensure_known_subcommand_tool(&["dylint".to_string()], &paths)
            .await?;
    for dir in &bootstrap.bin_dirs {
        eprintln!("soldr: dylint prepare: tool bin dir {}", dir.display());
    }

    eprintln!("soldr: dylint prepare: resolving the Dylint toolchain plan");
    let plan = crate::dylint_toolchain::resolve_plan(None, &workspace_root).await?;
    eprintln!(
        "soldr: dylint prepare: channel {} (compiler {} {})",
        plan.channel, plan.compiler_release, plan.compiler_commit
    );

    eprintln!("soldr: dylint prepare: verifying the prebuilt driver");
    crate::dylint_driver::ensure_prebuilt_driver(&plan, &paths).await?;
    let plan = crate::dylint_toolchain::prepare_resolved(plan)?;
    println!(
        "soldr: dylint ready on channel {} — the next `soldr cargo dylint` run starts warm",
        plan.channel
    );
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_rejects_arguments() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let error = runtime
            .block_on(run(&["--driver-version".to_string()]))
            .expect_err("arguments must be rejected");
        assert!(error.to_string().contains("takes no arguments"), "{error}");
    }
}
