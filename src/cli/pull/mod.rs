use crate::api::RossumClient;
use crate::config::ProjectConfig;
use crate::paths::Paths;
use crate::progress::OverallProgress;
use crate::secrets::resolve_token;
use crate::state::Lockfile;
use anyhow::{anyhow, Context, Result};
use std::sync::Arc;

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

/// Per-pull statistics aggregated across all driver runs.
struct PullStats {
    n_orgs: usize,
    n_workspaces: usize,
    qc: queues::QueueCounts,
    n_hooks: usize,
    c_hooks: usize,
    n_rules: usize,
    c_rules: usize,
    n_labels: usize,
    c_labels: usize,
    n_engines: usize,
    c_engines: usize,
    n_engine_fields: usize,
    c_engine_fields: usize,
    n_workflows: usize,
    c_workflows: usize,
    n_workflow_steps: usize,
    c_workflow_steps: usize,
    n_email_templates: usize,
    c_email_templates: usize,
    n_datasets: usize,
    c_datasets: usize,
    c_orgs: usize,
}

pub async fn run(env: &str, interactive: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let paths = Paths::for_env(&cwd, env);

    let cfg = ProjectConfig::load(&paths.project_config())?;

    let env_cfg = cfg
        .envs
        .get(env)
        .ok_or_else(|| anyhow!("env '{env}' is not defined in rdc.toml"))?;

    let token = resolve_token(&cwd, env)?;
    let client = RossumClient::new(env_cfg.api_base.clone(), token.clone())
        .context("constructing Rossum API client")?;

    let mut lockfile = Lockfile::load(&paths.lockfile())?;
    let overlay = crate::overlay::Overlay::load(&paths.overlay_file())
        .with_context(|| format!("loading overlay from {}", paths.overlay_file().display()))?;

    let progress = OverallProgress::start(format!("pull envs/{env}"));

    // Run drivers in a separate scope so the &mut borrow of `lockfile`
    // ends before we save it. If the user picks `[a]bort` at any
    // resolver prompt, a `PullAborted` error bubbles up; we detect it
    // via `error.chain()` (anyhow's `with_context` wraps it) and exit
    // cleanly without saving (spec §8.3 "rolls back lockfile; nothing
    // written").
    let pull_outcome = {
        let mut ctx = PullCtx {
            paths: &paths,
            client: &client,
            lockfile: &mut lockfile,
            queue_locations: std::collections::BTreeMap::new(),
            overlay,
            interactive,
        };
        run_drivers(&mut ctx, env_cfg, env, &token, &progress).await
    };

    let stats = match pull_outcome {
        Ok(s) => s,
        Err(e) if is_aborted(&e) => {
            progress.finish();
            eprintln!("pull aborted by user at conflict resolver; lockfile not saved.");
            return Ok(());
        }
        Err(e) => return Err(e),
    };

    let total_conflicts = stats.c_orgs + stats.c_hooks + stats.c_rules + stats.c_labels
        + stats.c_engines + stats.c_engine_fields + stats.c_workflows
        + stats.c_workflow_steps + stats.c_email_templates + stats.qc.conflicts
        + stats.c_datasets;

    lockfile.save(&paths.lockfile())?;
    crate::cli::index::generate(&paths, &lockfile)
        .with_context(|| format!("generating _index.md for env '{env}'"))?;

    let orphans = progress.orphans();
    progress.finish();

    let mut summary = format!(
        "Pulled {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}",
        common::pluralize(stats.n_orgs, "organization", "organizations"),
        common::pluralize(stats.n_workspaces, "workspace", "workspaces"),
        common::pluralize(stats.qc.queues, "queue", "queues"),
        common::pluralize(stats.qc.schemas, "schema", "schemas"),
        common::pluralize(stats.qc.inboxes, "inbox", "inboxes"),
        common::pluralize(stats.n_hooks, "hook", "hooks"),
        common::pluralize(stats.n_rules, "rule", "rules"),
        common::pluralize(stats.n_labels, "label", "labels"),
        common::pluralize(stats.n_engines, "engine", "engines"),
        common::pluralize(stats.n_engine_fields, "engine field", "engine fields"),
        common::pluralize(stats.n_workflows, "workflow", "workflows"),
        common::pluralize(stats.n_workflow_steps, "workflow step", "workflow steps"),
        common::pluralize(stats.n_email_templates, "email template", "email templates"),
    );
    // MDH is always attempted; we surface its count whenever any
    // datasets came back (or stay quiet when the cluster has no MDH /
    // returned 404).
    if stats.n_datasets > 0 {
        summary.push_str(&format!(", {}", common::pluralize(stats.n_datasets, "dataset", "datasets")));
    }
    if orphans > 0 {
        summary.push_str(&format!(", {} orphans skipped", orphans));
    }
    if total_conflicts > 0 {
        summary.push_str(&format!(", {}", common::pluralize(total_conflicts, "conflict", "conflicts")));
    }
    summary.push_str(&format!(" from env '{env}'"));
    println!("{summary}");

    // Stale slugs surface here: detected, never auto-applied. The user
    // runs `rdc repair <env> --rename-slugs` when ready to commit the moves.
    let pending = crate::cli::deploy::realign::detect(&paths, &lockfile);
    if !pending.is_empty() {
        eprintln!(
            "note: {} resource(s) have been renamed on remote — run `rdc repair {env} --rename-slugs` to apply",
            pending.len()
        );
    }
    Ok(())
}

/// Run every per-kind driver in two phases:
///
/// Phase 1: list all kinds from the API. The progress bar total denominator
/// is set in full before any processing begins, so the percentage only grows.
///
/// Phase 2: process (write to disk) all listed items in dependency order.
async fn run_drivers(
    ctx: &mut PullCtx<'_>,
    env_cfg: &crate::config::EnvConfig,
    env: &str,
    token: &str,
    progress: &Arc<OverallProgress>,
) -> Result<PullStats> {
    // ── Phase 1: list all kinds upfront ──────────────────────────────────────
    // The bar's total denominator accumulates here. No ticks happen yet.

    let org_listed = organization::list(ctx, env_cfg.org_id, progress).await
        .with_context(|| format!("listing organization for env '{env}'"))?;
    progress.inc_total(1); // singleton

    let workspaces_listed = workspaces::list(ctx, progress).await
        .with_context(|| format!("listing workspaces for env '{env}'"))?;
    progress.inc_total(workspaces_listed.len() as u64);

    let queues_listed = queues::list(ctx, progress).await
        .with_context(|| format!("listing queues for env '{env}'"))?;
    progress.inc_total(queues_listed.len() as u64);

    let hooks_listed = hooks::list(ctx, progress).await
        .with_context(|| format!("listing hooks for env '{env}'"))?;
    progress.inc_total(hooks_listed.len() as u64);

    let rules_listed = rules::list(ctx, progress).await
        .with_context(|| format!("listing rules for env '{env}'"))?;
    progress.inc_total(rules_listed.len() as u64);

    let labels_listed = labels::list(ctx, progress).await
        .with_context(|| format!("listing labels for env '{env}'"))?;
    progress.inc_total(labels_listed.len() as u64);

    let engines_listed = engines::list(ctx, progress).await
        .with_context(|| format!("listing engines for env '{env}'"))?;
    progress.inc_total(engines_listed.len() as u64);

    let engine_fields_listed = engine_fields::list(ctx, progress).await
        .with_context(|| format!("listing engine fields for env '{env}'"))?;
    progress.inc_total(engine_fields_listed.len() as u64);

    let workflows_listed = workflows::list(ctx, progress).await
        .with_context(|| format!("listing workflows for env '{env}'"))?;
    progress.inc_total(workflows_listed.len() as u64);

    let workflow_steps_listed = workflow_steps::list(ctx, progress).await
        .with_context(|| format!("listing workflow steps for env '{env}'"))?;
    progress.inc_total(workflow_steps_listed.len() as u64);

    let email_templates_listed = email_templates::list(ctx, progress).await
        .with_context(|| format!("listing email templates for env '{env}'"))?;
    progress.inc_total(email_templates_listed.len() as u64);

    let mdh_listed = mdh::list(env_cfg, token, progress).await
        .with_context(|| format!("listing MDH datasets for env '{env}'"))?;
    progress.inc_total(mdh_listed.collections.len() as u64);

    // ── Phase 2: process all kinds in dependency order ────────────────────────
    // Bar percentage only grows from here. queue_locations is populated by
    // queues::process and consumed by email_templates::process.

    let (n_orgs, c_orgs) = organization::process(ctx, org_listed, progress).await
        .with_context(|| format!("pulling organization for env '{env}'"))?;

    let n_workspaces = workspaces::process(ctx, workspaces_listed, progress).await
        .with_context(|| format!("pulling workspaces for env '{env}'"))?;

    let qc = queues::process(ctx, queues_listed, progress).await
        .with_context(|| format!("pulling queues for env '{env}'"))?;

    let (n_hooks, c_hooks) = hooks::process(ctx, hooks_listed, progress).await
        .with_context(|| format!("pulling hooks for env '{env}'"))?;

    let (n_rules, c_rules) = rules::process(ctx, rules_listed, progress).await
        .with_context(|| format!("pulling rules for env '{env}'"))?;

    let (n_labels, c_labels) = labels::process(ctx, labels_listed, progress).await
        .with_context(|| format!("pulling labels for env '{env}'"))?;

    let (n_engines, c_engines) = engines::process(ctx, engines_listed, progress).await
        .with_context(|| format!("pulling engines for env '{env}'"))?;

    let (n_engine_fields, c_engine_fields) = engine_fields::process(ctx, engine_fields_listed, progress).await
        .with_context(|| format!("pulling engine fields for env '{env}'"))?;

    let (n_workflows, c_workflows) = workflows::process(ctx, workflows_listed, progress).await
        .with_context(|| format!("pulling workflows for env '{env}'"))?;

    let (n_workflow_steps, c_workflow_steps) = workflow_steps::process(ctx, workflow_steps_listed, progress).await
        .with_context(|| format!("pulling workflow steps for env '{env}'"))?;

    // email_templates reads ctx.queue_locations which queues::process populated above.
    let (n_email_templates, c_email_templates) = email_templates::process(ctx, email_templates_listed, progress).await
        .with_context(|| format!("pulling email templates for env '{env}'"))?;

    let (n_datasets, c_datasets) = mdh::process(ctx, mdh_listed, progress).await
        .with_context(|| format!("pulling MDH datasets for env '{env}'"))?;

    Ok(PullStats {
        n_orgs, c_orgs,
        n_workspaces,
        qc,
        n_hooks, c_hooks,
        n_rules, c_rules,
        n_labels, c_labels,
        n_engines, c_engines,
        n_engine_fields, c_engine_fields,
        n_workflows, c_workflows,
        n_workflow_steps, c_workflow_steps,
        n_email_templates, c_email_templates,
        n_datasets, c_datasets,
    })
}

/// Walk the anyhow error chain looking for a `PullAborted` cause. Used
/// to detect "user picked [a]bort" through a stack of `with_context`
/// wrappers.
fn is_aborted(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<crate::cli::resolve::PullAborted>().is_some())
}
