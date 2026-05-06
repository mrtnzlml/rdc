use crate::api::RossumClient;
use crate::config::ProjectConfig;
use crate::paths::Paths;
use crate::secrets::resolve_token;
use crate::state::Lockfile;
use anyhow::{anyhow, Context, Result};

mod common;
mod hooks;
mod organization;
mod workspaces;

pub use common::PullCtx;

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
    let mut ctx = PullCtx { paths: &paths, client: &client, lockfile: &mut lockfile };

    let n_orgs = organization::pull(&mut ctx, env_cfg.org_id).await
        .with_context(|| format!("pulling organization for env '{env}'"))?;
    let n_workspaces = workspaces::pull(&mut ctx).await
        .with_context(|| format!("pulling workspaces for env '{env}'"))?;
    let n_hooks = hooks::pull(&mut ctx).await
        .with_context(|| format!("pulling hooks for env '{env}'"))?;

    lockfile.save(&paths.lockfile())?;
    println!(
        "Pulled {n_orgs} organization, {n_workspaces} workspaces, {n_hooks} hooks from env '{env}'"
    );
    Ok(())
}
