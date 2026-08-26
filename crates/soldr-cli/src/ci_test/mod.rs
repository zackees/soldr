//! Native host-validation DAG for `soldr ci-test` (soldr#2867).

mod execute;
mod model;
mod parse;
mod plan;

use crate::core::SoldrError;
use model::OutputFormat;

pub(crate) async fn run(
    args: &[String],
    cache_enabled: bool,
    trust_inherited_soldr_env: bool,
) -> Result<i32, SoldrError> {
    let invocation = parse::parse(args)?;
    let plan = plan::freeze(&invocation, cache_enabled).await?;
    if invocation.explain {
        render(&plan, invocation.format)?;
        return Ok(0);
    }
    execute::run(&plan, cache_enabled, trust_inherited_soldr_env).await
}

fn render(plan: &model::CiTestPlan, format: OutputFormat) -> Result<(), SoldrError> {
    match format {
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(plan).map_err(|error| {
                SoldrError::Other(format!(
                    "soldr ci-test: could not encode plan JSON: {error}"
                ))
            })?
        ),
        OutputFormat::Human => {
            println!("soldr ci-test plan v{}", plan.schema_version);
            println!("  workspace: {}", plan.workspace_root);
            println!("  host: {}", plan.host_triple);
            println!("  domains:");
            for domain in &plan.domains {
                println!(
                    "    {} ({}) {} -> {} [{}]",
                    domain.id,
                    domain.family,
                    domain.toolchain,
                    domain.target_directory,
                    domain.profile
                );
            }
            println!("  stages:");
            for stage in &plan.stages {
                println!("    {}: {}", stage.name, stage.command.join(" "));
            }
            for step in &plan.subsumed_steps {
                println!("  {}: {}", step.name, step.subsumed_by);
            }
        }
    }
    Ok(())
}
