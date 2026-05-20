use crate::api::RossumClient;
use crate::paths::Paths;
use crate::progress::SyncRenderer;
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
///
/// Each per-kind driver owns its own `Phase` from the shared
/// `ProgressLog`; dispatch order matches the dependency graph
/// (workspaces → schemas → queues → queue-children → org-level leaves).
///
/// `catalog_hooks` is the Phase-1 hook list, threaded into the hooks
/// driver so its store-extension orphan check can avoid a redundant
/// `list_hooks` call. Per-PATCH drift checks still re-list independently.
pub(crate) async fn push_classified(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
    env: &str,
    interactive: bool,
    changes: &scan::ChangeList,
    catalog_hooks: &[crate::model::Hook],
    progress: &Arc<dyn SyncRenderer>,
) -> Result<PushCounts> {
    let (n_workspaces, c_workspaces) = if !changes.workspaces.is_empty() {
        workspaces::push(paths, client, lockfile, interactive, &changes.workspaces, progress, env).await
            .with_context(|| format!("pushing workspaces for env '{env}'"))?
    } else {
        (0, 0)
    };

    let (n_schemas, c_schemas) = if !changes.schemas.is_empty() {
        schemas::push(paths, client, lockfile, interactive, &changes.schemas, progress, env).await
            .with_context(|| format!("pushing schemas for env '{env}'"))?
    } else {
        (0, 0)
    };

    let (n_queues, c_queues) = if !changes.queues.is_empty() {
        queues::push(paths, client, lockfile, interactive, &changes.queues, progress, env).await
            .with_context(|| format!("pushing queues for env '{env}'"))?
    } else {
        (0, 0)
    };

    let (n_inboxes, c_inboxes) = if !changes.inboxes.is_empty() {
        inboxes::push(paths, client, lockfile, interactive, &changes.inboxes, progress, env).await
            .with_context(|| format!("pushing inboxes for env '{env}'"))?
    } else {
        (0, 0)
    };

    let (n_email_templates, c_email_templates) = if !changes.email_templates.is_empty() {
        email_templates::push(paths, client, lockfile, interactive, &changes.email_templates, progress, env).await
            .with_context(|| format!("pushing email templates for env '{env}'"))?
    } else {
        (0, 0)
    };

    // Hooks always go through `push` (no early-skip on empty `changes`)
    // so the secrets-only pass inside it can detect changes to
    // `secrets/<env>.hook-secrets.json` that aren't accompanied by a
    // hook JSON/code edit. The function returns (0, 0) when neither
    // content nor secrets have drifted.
    let (n_hooks, c_hooks) =
        hooks::push(paths, client, lockfile, interactive, &changes.hooks, catalog_hooks, progress, env)
            .await
            .with_context(|| format!("pushing hooks for env '{env}'"))?;

    let (n_rules, c_rules) = if !changes.rules.is_empty() {
        rules::push(paths, client, lockfile, interactive, &changes.rules, progress, env).await
            .with_context(|| format!("pushing rules for env '{env}'"))?
    } else {
        (0, 0)
    };

    let (n_labels, c_labels) = if !changes.labels.is_empty() {
        labels::push(paths, client, lockfile, interactive, &changes.labels, progress, env).await
            .with_context(|| format!("pushing labels for env '{env}'"))?
    } else {
        (0, 0)
    };

    let (n_engines, c_engines) = if !changes.engines.is_empty() {
        engines::push(paths, client, lockfile, interactive, &changes.engines, progress, env).await
            .with_context(|| format!("pushing engines for env '{env}'"))?
    } else {
        (0, 0)
    };

    let (n_engine_fields, c_engine_fields) = if !changes.engine_fields.is_empty() {
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
