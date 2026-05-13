use crate::api::RossumClient;
use crate::config::ProjectConfig;
use crate::paths::Paths;
use crate::progress::OverallProgress;
use crate::secrets::resolve_token;
use crate::state::Lockfile;
use anyhow::{anyhow, Context, Result};
use std::sync::Arc;

mod email_templates;
mod engine_fields;
mod engines;
mod hooks;
mod inboxes;
mod labels;
mod queues;
mod rules;
pub mod scan;
mod schemas;
mod workspaces;

pub async fn run(env: &str, interactive: bool, dry_run: bool) -> Result<()> {
    let push_started = std::time::Instant::now();
    let cwd = std::env::current_dir().context("getting current directory")?;
    let paths = Paths::for_env(&cwd, env);

    let cfg = ProjectConfig::load(&paths.project_config())?;

    let env_cfg = cfg
        .envs
        .get(env)
        .ok_or_else(|| anyhow!("env '{env}' is not defined in rdc.toml"))?;

    let token = resolve_token(&cwd, env)?;
    let client = RossumClient::new(env_cfg.api_base.clone(), token)
        .context("constructing Rossum API client")?;

    let mut lockfile = Lockfile::load(&paths.lockfile())?;

    // Phase 1: scan local files for changes.
    eprintln!("→ push envs/{env}: scanning files…");
    let (scanned, changes) = scan::scan(&paths, &lockfile)?;
    eprintln!("✓ push envs/{env}: {scanned} files scanned, {} changed", changes.total());

    if changes.is_empty() {
        eprintln!(
            "✓ push envs/{env}: no changes  ({:.1}s)",
            push_started.elapsed().as_secs_f32()
        );
        return Ok(());
    }

    if dry_run {
        // Surface the per-kind breakdown so the user knows exactly what
        // a real push would touch. POST-vs-PATCH classification is
        // deferred (it depends on lockfile entries) but the count of
        // candidate files per kind is the meaningful preview.
        let kinds = [
            ("workspaces", &changes.workspaces),
            ("schemas", &changes.schemas),
            ("queues", &changes.queues),
            ("inboxes", &changes.inboxes),
            ("email_templates", &changes.email_templates),
            ("hooks", &changes.hooks),
            ("rules", &changes.rules),
            ("labels", &changes.labels),
            ("engines", &changes.engines),
            ("engine_fields", &changes.engine_fields),
        ];
        for (name, m) in kinds {
            if !m.is_empty() {
                println!("  → {name:18} {} would be POSTed/PATCHed", m.len());
                for slug in m.keys() {
                    println!("      {slug}");
                }
            }
        }
        println!(
            "Dry run push envs/{env}: {} change(s), {:.1}s — no API calls made.",
            changes.total(),
            push_started.elapsed().as_secs_f32()
        );
        return Ok(());
    }

    let progress = OverallProgress::start(format!("push envs/{env}"));

    // Run drivers in a separate function so we can detect [a]bort
    // (PullAborted) and skip lockfile.save(). Mirrors the pull-side
    // abort flow (spec §8.3 "rolls back lockfile; nothing written").
    let push_outcome = run_drivers(&paths, &client, &mut lockfile, env, interactive, &changes, &progress).await;

    let counts = match push_outcome {
        Ok(c) => c,
        Err(e) if is_aborted(&e) => {
            progress.finish();
            eprintln!("push aborted by user at conflict resolver; lockfile not saved.");
            return Ok(());
        }
        Err(e) => return Err(e),
    };

    progress.finish();
    lockfile.save(&paths.lockfile())?;
    crate::cli::index::generate(&paths, &lockfile)
        .with_context(|| format!("regenerating _index.md for env '{env}'"))?;

    let mut summary = format!(
        "Pushed {}, {}, {}, {}, {}, {}, {}, {}, {}, {} to env '{env}'",
        crate::cli::pull::common::pluralize(counts.n_workspaces, "workspace", "workspaces"),
        crate::cli::pull::common::pluralize(counts.n_hooks, "hook", "hooks"),
        crate::cli::pull::common::pluralize(counts.n_rules, "rule", "rules"),
        crate::cli::pull::common::pluralize(counts.n_labels, "label", "labels"),
        crate::cli::pull::common::pluralize(counts.n_queues, "queue", "queues"),
        crate::cli::pull::common::pluralize(counts.n_schemas, "schema", "schemas"),
        crate::cli::pull::common::pluralize(counts.n_inboxes, "inbox", "inboxes"),
        crate::cli::pull::common::pluralize(counts.n_email_templates, "email template", "email templates"),
        crate::cli::pull::common::pluralize(counts.n_engines, "engine", "engines"),
        crate::cli::pull::common::pluralize(counts.n_engine_fields, "engine field", "engine fields"),
    );
    let total_skipped = counts.c_workspaces + counts.c_hooks + counts.c_rules + counts.c_labels
        + counts.c_queues + counts.c_schemas + counts.c_inboxes + counts.c_email_templates
        + counts.c_engines + counts.c_engine_fields;
    if total_skipped > 0 {
        summary.push_str(&format!(", {} skipped (conflict)", total_skipped));
    }
    println!("{summary}");
    Ok(())
}

struct PushCounts {
    n_workspaces: usize, c_workspaces: usize,
    n_hooks: usize, c_hooks: usize,
    n_rules: usize, c_rules: usize,
    n_labels: usize, c_labels: usize,
    n_queues: usize, c_queues: usize,
    n_schemas: usize, c_schemas: usize,
    n_inboxes: usize, c_inboxes: usize,
    n_email_templates: usize, c_email_templates: usize,
    n_engines: usize, c_engines: usize,
    n_engine_fields: usize, c_engine_fields: usize,
}

async fn run_drivers(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
    env: &str,
    interactive: bool,
    changes: &scan::ChangeList,
    progress: &Arc<OverallProgress>,
) -> Result<PushCounts> {
    // Phase 1: accumulate the bar's total denominator for all changed kinds
    // upfront so the percentage only grows monotonically during phase 2.
    progress.inc_total(changes.workspaces.len() as u64);
    progress.inc_total(changes.hooks.len() as u64);
    progress.inc_total(changes.rules.len() as u64);
    progress.inc_total(changes.labels.len() as u64);
    progress.inc_total(changes.queues.len() as u64);
    progress.inc_total(changes.schemas.len() as u64);
    progress.inc_total(changes.inboxes.len() as u64);
    progress.inc_total(changes.email_templates.len() as u64);
    progress.inc_total(changes.engines.len() as u64);
    progress.inc_total(changes.engine_fields.len() as u64);

    // Phase 2: push each kind in dependency order. Workspaces first
    // (queues / schemas / inboxes / email_templates all root from a
    // workspace URL); schemas next (queues reference schema URLs); then
    // queues; then queue-children (inboxes, email_templates); then the
    // org-level leaves. Drivers call progress.tick() per item.
    let (n_workspaces, c_workspaces) = if !changes.workspaces.is_empty() {
        progress.start_phase("workspaces");
        workspaces::push(paths, client, lockfile, interactive, &changes.workspaces, progress).await
            .with_context(|| format!("pushing workspaces for env '{env}'"))?
    } else {
        (0, 0)
    };

    let (n_schemas, c_schemas) = if !changes.schemas.is_empty() {
        progress.start_phase("schemas");
        schemas::push(paths, client, lockfile, interactive, &changes.schemas, progress).await
            .with_context(|| format!("pushing schemas for env '{env}'"))?
    } else {
        (0, 0)
    };

    let (n_queues, c_queues) = if !changes.queues.is_empty() {
        progress.start_phase("queues");
        queues::push(paths, client, lockfile, interactive, &changes.queues, progress).await
            .with_context(|| format!("pushing queues for env '{env}'"))?
    } else {
        (0, 0)
    };

    let (n_inboxes, c_inboxes) = if !changes.inboxes.is_empty() {
        progress.start_phase("inboxes");
        inboxes::push(paths, client, lockfile, interactive, &changes.inboxes, progress).await
            .with_context(|| format!("pushing inboxes for env '{env}'"))?
    } else {
        (0, 0)
    };

    let (n_email_templates, c_email_templates) = if !changes.email_templates.is_empty() {
        progress.start_phase("email_templates");
        email_templates::push(paths, client, lockfile, interactive, &changes.email_templates, progress).await
            .with_context(|| format!("pushing email templates for env '{env}'"))?
    } else {
        (0, 0)
    };

    let (n_hooks, c_hooks) = if !changes.hooks.is_empty() {
        progress.start_phase("hooks");
        hooks::push(paths, client, lockfile, interactive, &changes.hooks, progress).await
            .with_context(|| format!("pushing hooks for env '{env}'"))?
    } else {
        (0, 0)
    };

    let (n_rules, c_rules) = if !changes.rules.is_empty() {
        progress.start_phase("rules");
        rules::push(paths, client, lockfile, interactive, &changes.rules, progress).await
            .with_context(|| format!("pushing rules for env '{env}'"))?
    } else {
        (0, 0)
    };

    let (n_labels, c_labels) = if !changes.labels.is_empty() {
        progress.start_phase("labels");
        labels::push(paths, client, lockfile, interactive, &changes.labels, progress).await
            .with_context(|| format!("pushing labels for env '{env}'"))?
    } else {
        (0, 0)
    };

    let (n_engines, c_engines) = if !changes.engines.is_empty() {
        progress.start_phase("engines");
        engines::push(paths, client, lockfile, interactive, &changes.engines, progress).await
            .with_context(|| format!("pushing engines for env '{env}'"))?
    } else {
        (0, 0)
    };

    let (n_engine_fields, c_engine_fields) = if !changes.engine_fields.is_empty() {
        progress.start_phase("engine_fields");
        engine_fields::push(paths, client, lockfile, interactive, &changes.engine_fields, progress).await
            .with_context(|| format!("pushing engine fields for env '{env}'"))?
    } else {
        (0, 0)
    };

    Ok(PushCounts {
        n_workspaces, c_workspaces,
        n_hooks, c_hooks,
        n_rules, c_rules,
        n_labels, c_labels,
        n_queues, c_queues,
        n_schemas, c_schemas,
        n_inboxes, c_inboxes,
        n_email_templates, c_email_templates,
        n_engines, c_engines,
        n_engine_fields, c_engine_fields,
    })
}

/// Walk the anyhow error chain looking for a `PullAborted` cause. Used
/// to detect "user picked [a]bort at the push drift resolver" through
/// `with_context` wrappers (mirrors pull/mod.rs).
fn is_aborted(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<crate::cli::resolve::PullAborted>().is_some())
}
