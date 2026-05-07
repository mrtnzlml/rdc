use crate::api::RossumClient;
use crate::config::ProjectConfig;
use crate::paths::Paths;
use crate::secrets::resolve_token;
use crate::state::Lockfile;
use anyhow::{anyhow, Context, Result};

pub(crate) mod common;
mod email_templates;
mod engine_fields;
mod engines;
mod hooks;
mod labels;
mod mdh;
mod organization;
mod queues;
mod rules;
mod workflow_steps;
mod workflows;
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
    let client = RossumClient::new(env_cfg.api_base.clone(), token.clone())
        .context("constructing Rossum API client")?;

    let mut lockfile = Lockfile::load(&paths.lockfile())?;
    let mut ctx = PullCtx {
        paths: &paths,
        client: &client,
        lockfile: &mut lockfile,
        queue_locations: std::collections::BTreeMap::new(),
    };

    // Flat-list kinds (M7 three-way detection):
    let (n_orgs, c_orgs) = organization::pull(&mut ctx, env_cfg.org_id).await
        .with_context(|| format!("pulling organization for env '{env}'"))?;
    let n_workspaces = workspaces::pull(&mut ctx, env_cfg).await
        .with_context(|| format!("pulling workspaces for env '{env}'"))?;
    let qc = queues::pull(&mut ctx).await
        .with_context(|| format!("pulling queues for env '{env}'"))?;
    let (n_hooks, c_hooks) = hooks::pull(&mut ctx).await
        .with_context(|| format!("pulling hooks for env '{env}'"))?;
    let (n_rules, c_rules) = rules::pull(&mut ctx).await
        .with_context(|| format!("pulling rules for env '{env}'"))?;
    let (n_labels, c_labels) = labels::pull(&mut ctx).await
        .with_context(|| format!("pulling labels for env '{env}'"))?;
    let (n_engines, c_engines) = engines::pull(&mut ctx).await
        .with_context(|| format!("pulling engines for env '{env}'"))?;
    let (n_engine_fields, c_engine_fields) = engine_fields::pull(&mut ctx).await
        .with_context(|| format!("pulling engine fields for env '{env}'"))?;
    let (n_workflows, c_workflows) = workflows::pull(&mut ctx).await
        .with_context(|| format!("pulling workflows for env '{env}'"))?;
    let (n_workflow_steps, c_workflow_steps) = workflow_steps::pull(&mut ctx).await
        .with_context(|| format!("pulling workflow steps for env '{env}'"))?;
    let (n_email_templates, c_email_templates) = email_templates::pull(&mut ctx).await
        .with_context(|| format!("pulling email templates for env '{env}'"))?;
    let (n_datasets, c_datasets) = mdh::pull(&mut ctx, env_cfg, &token).await
        .with_context(|| format!("pulling MDH datasets for env '{env}'"))?;

    let total_conflicts = c_orgs + c_hooks + c_rules + c_labels + c_engines
        + c_engine_fields + c_workflows + c_workflow_steps + c_email_templates
        + qc.conflicts + c_datasets;

    lockfile.save(&paths.lockfile())?;
    crate::cli::index::generate(&paths, &lockfile)
        .with_context(|| format!("generating _index.md for env '{env}'"))?;
    let mut summary = format!(
        "Pulled {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}",
        common::pluralize(n_orgs, "organization", "organizations"),
        common::pluralize(n_workspaces, "workspace", "workspaces"),
        common::pluralize(qc.queues, "queue", "queues"),
        common::pluralize(qc.schemas, "schema", "schemas"),
        common::pluralize(qc.inboxes, "inbox", "inboxes"),
        common::pluralize(n_hooks, "hook", "hooks"),
        common::pluralize(n_rules, "rule", "rules"),
        common::pluralize(n_labels, "label", "labels"),
        common::pluralize(n_engines, "engine", "engines"),
        common::pluralize(n_engine_fields, "engine field", "engine fields"),
        common::pluralize(n_workflows, "workflow", "workflows"),
        common::pluralize(n_workflow_steps, "workflow step", "workflow steps"),
        common::pluralize(n_email_templates, "email template", "email templates"),
    );
    if env_cfg.data_storage_base.is_some() {
        summary.push_str(&format!(", {}", common::pluralize(n_datasets, "dataset", "datasets")));
    }
    if total_conflicts > 0 {
        summary.push_str(&format!(", {}", common::pluralize(total_conflicts, "conflict", "conflicts")));
    }
    summary.push_str(&format!(" from env '{env}'"));
    println!("{summary}");
    Ok(())
}
