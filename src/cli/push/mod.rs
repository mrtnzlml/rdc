use crate::api::RossumClient;
use crate::config::ProjectConfig;
use crate::paths::Paths;
use crate::secrets::resolve_token;
use crate::state::Lockfile;
use anyhow::{anyhow, Context, Result};

mod hooks;
mod labels;
mod rules;
mod schemas;

pub async fn run(env: &str) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let paths = Paths::for_env(&cwd, env);

    let cfg = ProjectConfig::load(&paths.project_config())
        .with_context(|| format!("loading project config from {}", paths.project_config().display()))?;

    let env_cfg = cfg
        .envs
        .get(env)
        .ok_or_else(|| anyhow!("env '{env}' is not defined in rdc.toml"))?;

    let token = resolve_token(&cwd, env)?;
    let client = RossumClient::new(env_cfg.api_base.clone(), token)
        .context("constructing Rossum API client")?;

    let mut lockfile = Lockfile::load(&paths.lockfile())?;

    let (n_hooks, c_hooks) = hooks::push(&paths, &client, &mut lockfile).await
        .with_context(|| format!("pushing hooks for env '{env}'"))?;
    let (n_rules, c_rules) = rules::push(&paths, &client, &mut lockfile).await
        .with_context(|| format!("pushing rules for env '{env}'"))?;
    let (n_labels, c_labels) = labels::push(&paths, &client, &mut lockfile).await
        .with_context(|| format!("pushing labels for env '{env}'"))?;
    let (n_schemas, c_schemas) = schemas::push(&paths, &client, &mut lockfile).await
        .with_context(|| format!("pushing schemas for env '{env}'"))?;

    lockfile.save(&paths.lockfile())?;
    crate::cli::index::generate(&paths, &lockfile)
        .with_context(|| format!("regenerating _index.md for env '{env}'"))?;

    let mut summary = format!(
        "Pushed {}, {}, {}, {} to env '{env}'",
        crate::cli::pull::common::pluralize(n_hooks, "hook", "hooks"),
        crate::cli::pull::common::pluralize(n_rules, "rule", "rules"),
        crate::cli::pull::common::pluralize(n_labels, "label", "labels"),
        crate::cli::pull::common::pluralize(n_schemas, "schema", "schemas"),
    );
    let total_skipped = c_hooks + c_rules + c_labels + c_schemas;
    if total_skipped > 0 {
        summary.push_str(&format!(", {} skipped (conflict)", total_skipped));
    }
    println!("{summary}");
    Ok(())
}
