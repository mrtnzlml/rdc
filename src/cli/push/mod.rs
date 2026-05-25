use crate::api::RossumClient;
use crate::log::Log;
use crate::paths::Paths;
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
    progress: &Arc<Log>,
) -> Result<()> {
    if !changes.workspaces.is_empty() {
        workspaces::push(paths, client, lockfile, interactive, &changes.workspaces, progress, env).await
            .with_context(|| format!("pushing workspaces for env '{env}'"))?;
    }
    if !changes.schemas.is_empty() {
        schemas::push(paths, client, lockfile, interactive, &changes.schemas, progress, env).await
            .with_context(|| format!("pushing schemas for env '{env}'"))?;
    }
    if !changes.queues.is_empty() {
        queues::push(paths, client, lockfile, interactive, &changes.queues, progress, env).await
            .with_context(|| format!("pushing queues for env '{env}'"))?;
    }
    if !changes.inboxes.is_empty() {
        inboxes::push(paths, client, lockfile, interactive, &changes.inboxes, progress, env).await
            .with_context(|| format!("pushing inboxes for env '{env}'"))?;
    }
    if !changes.email_templates.is_empty() {
        email_templates::push(paths, client, lockfile, interactive, &changes.email_templates, progress, env).await
            .with_context(|| format!("pushing email templates for env '{env}'"))?;
    }
    // Hooks always go through `push` (no early-skip on empty `changes`)
    // so the secrets-only pass inside it can detect changes to
    // `secrets/<env>.hook-secrets.json` that aren't accompanied by a
    // hook JSON/code edit. The function returns (0, 0) when neither
    // content nor secrets have drifted.
    hooks::push(paths, client, lockfile, interactive, &changes.hooks, catalog_hooks, progress, env)
        .await
        .with_context(|| format!("pushing hooks for env '{env}'"))?;
    if !changes.rules.is_empty() {
        rules::push(paths, client, lockfile, interactive, &changes.rules, progress, env).await
            .with_context(|| format!("pushing rules for env '{env}'"))?;
    }
    if !changes.labels.is_empty() {
        labels::push(paths, client, lockfile, interactive, &changes.labels, progress, env).await
            .with_context(|| format!("pushing labels for env '{env}'"))?;
    }
    if !changes.engines.is_empty() {
        engines::push(paths, client, lockfile, interactive, &changes.engines, progress, env).await
            .with_context(|| format!("pushing engines for env '{env}'"))?;
    }
    if !changes.engine_fields.is_empty() {
        engine_fields::push(paths, client, lockfile, interactive, &changes.engine_fields, progress, env).await
            .with_context(|| format!("pushing engine fields for env '{env}'"))?;
    }
    Ok(())
}
