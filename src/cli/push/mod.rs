use crate::api::RossumClient;
use crate::config::ProjectConfig;
use crate::paths::Paths;
use crate::progress::KindProgress;
use crate::secrets::resolve_token;
use crate::state::Lockfile;
use anyhow::{anyhow, Context, Result};

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

pub async fn run(env: &str, interactive: bool) -> Result<()> {
    let push_started = std::time::Instant::now();
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

    // Phase 1: scan local files for changes.
    let scan_progress = KindProgress::start(format!("push envs/{env}"));
    let (scanned, changes) = scan::scan(&paths, &lockfile)?;
    drop(scan_progress); // suppress auto ✓ line; we emit our own.
    eprintln!("✓ push envs/{env}: {scanned} files scanned, {} changed", changes.total());

    if changes.is_empty() {
        eprintln!(
            "✓ push envs/{env}: no changes  ({:.1}s)",
            push_started.elapsed().as_secs_f32()
        );
        return Ok(());
    }

    // Run drivers in a separate function so we can detect [a]bort
    // (PullAborted) and skip lockfile.save(). Mirrors the pull-side
    // abort flow (spec §8.3 "rolls back lockfile; nothing written").
    let push_outcome = run_drivers(&paths, &client, &mut lockfile, env, interactive, &changes).await;

    let counts = match push_outcome {
        Ok(c) => c,
        Err(e) if is_aborted(&e) => {
            eprintln!("push aborted by user at conflict resolver; lockfile not saved.");
            return Ok(());
        }
        Err(e) => return Err(e),
    };

    lockfile.save(&paths.lockfile())?;
    crate::cli::index::generate(&paths, &lockfile)
        .with_context(|| format!("regenerating _index.md for env '{env}'"))?;

    let mut summary = format!(
        "Pushed {}, {}, {}, {}, {}, {}, {}, {}, {} to env '{env}'",
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
    let total_skipped = counts.c_hooks + counts.c_rules + counts.c_labels + counts.c_queues
        + counts.c_schemas + counts.c_inboxes + counts.c_email_templates
        + counts.c_engines + counts.c_engine_fields;
    if total_skipped > 0 {
        summary.push_str(&format!(", {} skipped (conflict)", total_skipped));
    }
    println!("{summary}");
    Ok(())
}

struct PushCounts {
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
) -> Result<PushCounts> {
    let (n_hooks, c_hooks) = if !changes.hooks.is_empty() {
        let p = KindProgress::start("hooks");
        p.set_total(changes.hooks.len() as u64);
        let result = hooks::push(paths, client, lockfile, interactive, &changes.hooks, &p).await
            .with_context(|| format!("pushing hooks for env '{env}'"))?;
        p.finish();
        result
    } else {
        (0, 0)
    };

    let (n_rules, c_rules) = if !changes.rules.is_empty() {
        let p = KindProgress::start("rules");
        p.set_total(changes.rules.len() as u64);
        let result = rules::push(paths, client, lockfile, interactive, &changes.rules, &p).await
            .with_context(|| format!("pushing rules for env '{env}'"))?;
        p.finish();
        result
    } else {
        (0, 0)
    };

    let (n_labels, c_labels) = if !changes.labels.is_empty() {
        let p = KindProgress::start("labels");
        p.set_total(changes.labels.len() as u64);
        let result = labels::push(paths, client, lockfile, interactive, &changes.labels, &p).await
            .with_context(|| format!("pushing labels for env '{env}'"))?;
        p.finish();
        result
    } else {
        (0, 0)
    };

    let (n_queues, c_queues) = if !changes.queues.is_empty() {
        let p = KindProgress::start("queues");
        p.set_total(changes.queues.len() as u64);
        let result = queues::push(paths, client, lockfile, interactive, &changes.queues, &p).await
            .with_context(|| format!("pushing queues for env '{env}'"))?;
        p.finish();
        result
    } else {
        (0, 0)
    };

    let (n_schemas, c_schemas) = if !changes.schemas.is_empty() {
        let p = KindProgress::start("schemas");
        p.set_total(changes.schemas.len() as u64);
        let result = schemas::push(paths, client, lockfile, interactive, &changes.schemas, &p).await
            .with_context(|| format!("pushing schemas for env '{env}'"))?;
        p.finish();
        result
    } else {
        (0, 0)
    };

    let (n_inboxes, c_inboxes) = if !changes.inboxes.is_empty() {
        let p = KindProgress::start("inboxes");
        p.set_total(changes.inboxes.len() as u64);
        let result = inboxes::push(paths, client, lockfile, interactive, &changes.inboxes, &p).await
            .with_context(|| format!("pushing inboxes for env '{env}'"))?;
        p.finish();
        result
    } else {
        (0, 0)
    };

    let (n_email_templates, c_email_templates) = if !changes.email_templates.is_empty() {
        let p = KindProgress::start("email_templates");
        p.set_total(changes.email_templates.len() as u64);
        let result = email_templates::push(paths, client, lockfile, interactive, &changes.email_templates, &p).await
            .with_context(|| format!("pushing email templates for env '{env}'"))?;
        p.finish();
        result
    } else {
        (0, 0)
    };

    let (n_engines, c_engines) = if !changes.engines.is_empty() {
        let p = KindProgress::start("engines");
        p.set_total(changes.engines.len() as u64);
        let result = engines::push(paths, client, lockfile, interactive, &changes.engines, &p).await
            .with_context(|| format!("pushing engines for env '{env}'"))?;
        p.finish();
        result
    } else {
        (0, 0)
    };

    let (n_engine_fields, c_engine_fields) = if !changes.engine_fields.is_empty() {
        let p = KindProgress::start("engine_fields");
        p.set_total(changes.engine_fields.len() as u64);
        let result = engine_fields::push(paths, client, lockfile, interactive, &changes.engine_fields, &p).await
            .with_context(|| format!("pushing engine fields for env '{env}'"))?;
        p.finish();
        result
    } else {
        (0, 0)
    };

    Ok(PushCounts {
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
