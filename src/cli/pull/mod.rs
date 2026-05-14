use crate::api::RossumClient;
use crate::config::ProjectConfig;
use crate::paths::Paths;
use crate::progress::OverallProgress;
use crate::secrets::resolve_token;
use crate::slug::slugify_unique;
use crate::state::Lockfile;
use anyhow::{anyhow, Context, Result};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

pub(crate) mod common;
mod email_templates;
mod engine_fields;
mod engines;
mod hooks;
pub(crate) mod labels;
pub(crate) mod mdh;
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
    // Listing logic lives in `common::list_remote` so the sync classifier
    // can reuse it; the order of calls is preserved there verbatim.
    let catalog = common::list_remote(ctx, env_cfg, env, token, progress).await?;

    // ── Phase 2: process all kinds in dependency order ────────────────────────
    // Bar percentage only grows from here. queue_locations is populated by
    // queues::process and consumed by email_templates::process.
    //
    // Build the all-inclusive subset by replaying the slug computation each
    // driver uses; pull always writes everything the catalog listed. The
    // sync classifier (Task 13+) builds a filtered subset instead.
    let all_subset = full_subset_from(&catalog, ctx.lockfile);

    let (n_orgs, c_orgs) = organization::process(ctx, catalog.organization, progress).await
        .with_context(|| format!("pulling organization for env '{env}'"))?;

    let n_workspaces = workspaces::process(ctx, catalog.workspaces, &all_subset, progress).await
        .with_context(|| format!("pulling workspaces for env '{env}'"))?;

    let qc = queues::process(ctx, catalog.queues, &all_subset, progress).await
        .with_context(|| format!("pulling queues for env '{env}'"))?;

    let (n_hooks, c_hooks) = hooks::process(ctx, catalog.hooks, &all_subset, progress).await
        .with_context(|| format!("pulling hooks for env '{env}'"))?;

    let (n_rules, c_rules) = rules::process(ctx, catalog.rules, &all_subset, progress).await
        .with_context(|| format!("pulling rules for env '{env}'"))?;

    let (n_labels, c_labels) = labels::process(ctx, catalog.labels, &all_subset, progress).await
        .with_context(|| format!("pulling labels for env '{env}'"))?;

    let (n_engines, c_engines) = engines::process(ctx, catalog.engines, &all_subset, progress).await
        .with_context(|| format!("pulling engines for env '{env}'"))?;

    let (n_engine_fields, c_engine_fields) = engine_fields::process(ctx, catalog.engine_fields, &all_subset, progress).await
        .with_context(|| format!("pulling engine fields for env '{env}'"))?;

    let (n_workflows, c_workflows) = workflows::process(ctx, catalog.workflows, &all_subset, progress).await
        .with_context(|| format!("pulling workflows for env '{env}'"))?;

    let (n_workflow_steps, c_workflow_steps) = workflow_steps::process(ctx, catalog.workflow_steps, &all_subset, progress).await
        .with_context(|| format!("pulling workflow steps for env '{env}'"))?;

    // email_templates reads ctx.queue_locations which queues::process populated above.
    let (n_email_templates, c_email_templates) = email_templates::process(ctx, catalog.email_templates, &all_subset, progress).await
        .with_context(|| format!("pulling email templates for env '{env}'"))?;

    let (n_datasets, c_datasets) = mdh::process(ctx, catalog.mdh, &all_subset, progress).await
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

/// Build a `(kind, slug)` set that names every item in the catalog. Each
/// driver's `process` consults this set to decide what to write; on a
/// regular pull we want to write everything that was listed, so the set
/// must contain every slug each driver would otherwise compute itself.
///
/// Slug derivation mirrors each driver's logic exactly: lockfile id → slug
/// lookup with a `slugify_unique` fallback. Per-parent collision sets
/// (queues per workspace, email templates per queue) follow the same
/// disambiguation order the drivers use.
fn full_subset_from(
    catalog: &common::RemoteCatalog,
    lockfile: &Lockfile,
) -> BTreeSet<(String, String)> {
    let mut s: BTreeSet<(String, String)> = BTreeSet::new();

    // workspaces — flat slug set, lockfile id lookup + slugify fallback.
    let mut ws_used: HashSet<String> = HashSet::new();
    let mut ws_url_to_slug: HashMap<String, String> = HashMap::new();
    for ws in &catalog.workspaces {
        let slug = lockfile.slug_for_id("workspaces", ws.id)
            .map(|s| s.to_string())
            .unwrap_or_else(|| slugify_unique(&ws.name, &ws_used));
        ws_used.insert(slug.clone());
        ws_url_to_slug.insert(ws.url.clone(), slug.clone());
        s.insert(("workspaces".to_string(), slug));
    }

    // queues — slug per workspace, mirrors `queues::process` orphan + ws-slug
    // lookup order. Queue slugs are also stashed by URL for email_templates.
    let mut q_per_ws_used: HashMap<String, HashSet<String>> = HashMap::new();
    let mut q_url_to_ws_q: HashMap<String, (String, String)> = HashMap::new();
    for q in &catalog.queues {
        let Some(ws_url) = q.workspace.as_ref() else { continue };
        // Prefer the freshly-computed catalog mapping; fall back to lockfile.
        let ws_slug = match ws_url_to_slug.get(ws_url) {
            Some(s) => s.clone(),
            None => match lockfile.slug_for_url("workspaces", ws_url) {
                Some(s) => s.to_string(),
                None => continue,
            },
        };
        let used = q_per_ws_used.entry(ws_slug.clone()).or_default();
        let q_slug = lockfile.slug_for_id("queues", q.id)
            .map(|s| s.to_string())
            .unwrap_or_else(|| slugify_unique(&q.name, used));
        used.insert(q_slug.clone());
        q_url_to_ws_q.insert(q.url.clone(), (ws_slug, q_slug.clone()));
        s.insert(("queues".to_string(), q_slug));
    }

    // hooks
    let mut h_used: HashSet<String> = HashSet::new();
    for h in &catalog.hooks {
        let slug = lockfile.slug_for_id("hooks", h.id)
            .map(|s| s.to_string())
            .unwrap_or_else(|| slugify_unique(&h.name, &h_used));
        h_used.insert(slug.clone());
        s.insert(("hooks".to_string(), slug));
    }

    // rules
    let mut r_used: HashSet<String> = HashSet::new();
    for r in &catalog.rules {
        let slug = lockfile.slug_for_id("rules", r.id)
            .map(|s| s.to_string())
            .unwrap_or_else(|| slugify_unique(&r.name, &r_used));
        r_used.insert(slug.clone());
        s.insert(("rules".to_string(), slug));
    }

    // labels
    let mut l_used: HashSet<String> = HashSet::new();
    for l in &catalog.labels {
        let slug = lockfile.slug_for_id("labels", l.id)
            .map(|s| s.to_string())
            .unwrap_or_else(|| slugify_unique(&l.name, &l_used));
        l_used.insert(slug.clone());
        s.insert(("labels".to_string(), slug));
    }

    // engines
    let mut e_used: HashSet<String> = HashSet::new();
    for e in &catalog.engines {
        let slug = lockfile.slug_for_id("engines", e.id)
            .map(|s| s.to_string())
            .unwrap_or_else(|| slugify_unique(&e.name, &e_used));
        e_used.insert(slug.clone());
        s.insert(("engines".to_string(), slug));
    }

    // engine_fields — flat `used` set; orphans (no engine in lockfile) are
    // skipped by the driver but still emit a slug here so the set is a
    // strict superset of what `process` would write.
    let mut ef_used: HashSet<String> = HashSet::new();
    for f in &catalog.engine_fields {
        let slug = lockfile.slug_for_id("engine_fields", f.id)
            .map(|s| s.to_string())
            .unwrap_or_else(|| slugify_unique(&f.name, &ef_used));
        ef_used.insert(slug.clone());
        s.insert(("engine_fields".to_string(), slug));
    }

    // workflows
    let mut w_used: HashSet<String> = HashSet::new();
    for w in &catalog.workflows {
        let slug = lockfile.slug_for_id("workflows", w.id)
            .map(|s| s.to_string())
            .unwrap_or_else(|| slugify_unique(&w.name, &w_used));
        w_used.insert(slug.clone());
        s.insert(("workflows".to_string(), slug));
    }

    // workflow_steps — flat `used` set, like engine_fields.
    let mut ws_step_used: HashSet<String> = HashSet::new();
    for step in &catalog.workflow_steps {
        let slug = lockfile.slug_for_id("workflow_steps", step.id)
            .map(|s| s.to_string())
            .unwrap_or_else(|| slugify_unique(&step.name, &ws_step_used));
        ws_step_used.insert(slug.clone());
        s.insert(("workflow_steps".to_string(), slug));
    }

    // email_templates — per-queue `used` map; compound `<ws>/<q>/<tpl>` key
    // matches the lockfile key the driver records.
    let mut et_per_queue_used: HashMap<(String, String), HashSet<String>> = HashMap::new();
    for t in &catalog.email_templates {
        let Some(queue_url) = t.queue.as_ref() else { continue };
        // Prefer the freshly-computed (ws,q) mapping; fall back to lockfile
        // (matches what queues::process eventually populates).
        let (ws_slug, q_slug) = match q_url_to_ws_q.get(queue_url) {
            Some(pair) => pair.clone(),
            None => match lockfile.slug_for_url("queues", queue_url) {
                Some(_q) => continue, // lockfile has the queue but we don't
                                       // know its workspace here; the driver
                                       // would also skip via queue_locations.
                None => continue,
            },
        };
        let used = et_per_queue_used.entry((ws_slug.clone(), q_slug.clone())).or_default();
        let template_slug = match lockfile.slug_for_id("email_templates", t.id) {
            Some(existing) => existing.rsplit('/').next().unwrap_or(existing).to_string(),
            None => slugify_unique(&t.name, used),
        };
        used.insert(template_slug.clone());
        s.insert((
            "email_templates".to_string(),
            format!("{ws_slug}/{q_slug}/{template_slug}"),
        ));
    }

    // mdh — slug is `slugify_unique(c.name)` (no lockfile lookup in
    // `mdh::process` today).
    let mut mdh_used: HashSet<String> = HashSet::new();
    for c in &catalog.mdh.collections {
        let slug = slugify_unique(&c.name, &mdh_used);
        mdh_used.insert(slug.clone());
        s.insert(("mdh".to_string(), slug));
    }

    s
}
