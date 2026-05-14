use crate::api::RossumClient;
use crate::paths::Paths;
use crate::progress::OverallProgress;
use crate::state::Lockfile;
use anyhow::{Context, Result};
use std::sync::Arc;

pub mod deletes;
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

/// Per-kind tallies produced by [`push_classified`]. Fields are
/// kept for future summary surfacing; the current sync flow discards
/// the value because the plan already enumerates what was written.
#[allow(dead_code)]
pub(crate) struct PushCounts {
    pub(crate) n_workspaces: usize, pub(crate) c_workspaces: usize,
    pub(crate) n_hooks: usize, pub(crate) c_hooks: usize,
    pub(crate) n_rules: usize, pub(crate) c_rules: usize,
    pub(crate) n_labels: usize, pub(crate) c_labels: usize,
    pub(crate) n_queues: usize, pub(crate) c_queues: usize,
    pub(crate) n_schemas: usize, pub(crate) c_schemas: usize,
    pub(crate) n_inboxes: usize, pub(crate) c_inboxes: usize,
    pub(crate) n_email_templates: usize, pub(crate) c_email_templates: usize,
    pub(crate) n_engines: usize, pub(crate) c_engines: usize,
    pub(crate) n_engine_fields: usize, pub(crate) c_engine_fields: usize,
}

/// Push phase: run each kind's push driver in dependency order. Called
/// by `cli::sync::execute` after the classifier identifies local edits
/// and creates; the executor builds a `ChangeList` from classified items
/// and delegates here.
pub(crate) async fn push_classified(
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
        workspaces::push(paths, client, lockfile, interactive, &changes.workspaces, progress, env).await
            .with_context(|| format!("pushing workspaces for env '{env}'"))?
    } else {
        (0, 0)
    };

    let (n_schemas, c_schemas) = if !changes.schemas.is_empty() {
        progress.start_phase("schemas");
        schemas::push(paths, client, lockfile, interactive, &changes.schemas, progress, env).await
            .with_context(|| format!("pushing schemas for env '{env}'"))?
    } else {
        (0, 0)
    };

    let (n_queues, c_queues) = if !changes.queues.is_empty() {
        progress.start_phase("queues");
        queues::push(paths, client, lockfile, interactive, &changes.queues, progress, env).await
            .with_context(|| format!("pushing queues for env '{env}'"))?
    } else {
        (0, 0)
    };

    let (n_inboxes, c_inboxes) = if !changes.inboxes.is_empty() {
        progress.start_phase("inboxes");
        inboxes::push(paths, client, lockfile, interactive, &changes.inboxes, progress, env).await
            .with_context(|| format!("pushing inboxes for env '{env}'"))?
    } else {
        (0, 0)
    };

    let (n_email_templates, c_email_templates) = if !changes.email_templates.is_empty() {
        progress.start_phase("email_templates");
        email_templates::push(paths, client, lockfile, interactive, &changes.email_templates, progress, env).await
            .with_context(|| format!("pushing email templates for env '{env}'"))?
    } else {
        (0, 0)
    };

    let (n_hooks, c_hooks) = if !changes.hooks.is_empty() {
        progress.start_phase("hooks");
        hooks::push(paths, client, lockfile, interactive, &changes.hooks, progress, env).await
            .with_context(|| format!("pushing hooks for env '{env}'"))?
    } else {
        (0, 0)
    };

    let (n_rules, c_rules) = if !changes.rules.is_empty() {
        progress.start_phase("rules");
        rules::push(paths, client, lockfile, interactive, &changes.rules, progress, env).await
            .with_context(|| format!("pushing rules for env '{env}'"))?
    } else {
        (0, 0)
    };

    let (n_labels, c_labels) = if !changes.labels.is_empty() {
        progress.start_phase("labels");
        labels::push(paths, client, lockfile, interactive, &changes.labels, progress, env).await
            .with_context(|| format!("pushing labels for env '{env}'"))?
    } else {
        (0, 0)
    };

    let (n_engines, c_engines) = if !changes.engines.is_empty() {
        progress.start_phase("engines");
        engines::push(paths, client, lockfile, interactive, &changes.engines, progress, env).await
            .with_context(|| format!("pushing engines for env '{env}'"))?
    } else {
        (0, 0)
    };

    let (n_engine_fields, c_engine_fields) = if !changes.engine_fields.is_empty() {
        progress.start_phase("engine_fields");
        engine_fields::push(paths, client, lockfile, interactive, &changes.engine_fields, progress, env).await
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
