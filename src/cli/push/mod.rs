use crate::api::RossumClient;
use crate::log::Log;
use crate::paths::Paths;
use crate::state::Lockfile;
use anyhow::{Context, Result};
use std::sync::Arc;

pub mod deletes;
pub mod relink;
mod concurrent;
mod email_templates;
mod engine_fields;
mod engines;
mod hooks;
/// Re-exported for the `--dry-run` planner in [`crate::cli::sync`], which needs
/// to predict the hooks secrets-only pass network-free.
pub(crate) use hooks::plan_secret_pushes;
mod inboxes;
mod labels;
pub mod mdh;
pub mod mdh_data;
mod organization;
mod queues;
mod rules;
mod saved_views;
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
/// (workspaces → engines → engine fields → schemas → queues → queue-children
/// → org-level leaves).
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
    relink: &mut Vec<relink::DeferredRelink>,
    progress: &Arc<Log>,
) -> Result<(usize, usize)> {
    // Aggregate (pushed, skipped) across every per-kind driver so the
    // cycle summary can reconcile its plan-time tally with what was
    // actually written (a driver skip — refused create, drift skip,
    // adopt-remote — is NOT a remote change).
    let mut pushed = 0usize;
    let mut skipped = 0usize;
    let mut tally = |counts: (usize, usize)| {
        pushed += counts.0;
        skipped += counts.1;
    };
    if !changes.workspaces.is_empty() {
        tally(workspaces::push(paths, client, lockfile, interactive, &changes.workspaces, progress, env).await
            .with_context(|| format!("pushing workspaces for env '{env}'"))?);
    }
    // Engines and their fields before schemas and queues. Unlike every other
    // edge in this graph, this one is not a reference the payload carries — it
    // is a server-side CONTENT check: `POST /queues` validates the queue's
    // schema against the bound engine's field NAMES and refuses the create with
    //
    //   non_field_errors: Engine (id: N) restriction: extracted field
    //   '<schema field>' is not present among names of engine fields
    //
    // for every extracted field the engine does not (yet) have. Pushed last, as
    // "org-level leaves", the engine fields did not exist yet and a promote into
    // a fresh env died on its first queue.
    //
    // The failure was hidden while the target engine was NEW: the queue's
    // `engine` ref could not resolve, so it was deferred out of the create body
    // and PATCHed by `run_relink` after the engine fields existed. It bites only
    // when the engine ALREADY exists in the target — the ref resolves, the
    // binding ships with the create, and it is validated against an engine whose
    // fields are still queued behind the queue.
    //
    // Safe this early: an engine's only queue-facing ref is `training_queues`,
    // which lives in `extra` and is therefore deferrable by construction (see
    // `relink::undeferrable`, where `engines` protects only `url`), so it is
    // postponed to the relink pass exactly as a hook's `run_after` is. Engine
    // fields reference only their engine, which is why they stay behind it.
    if !changes.engines.is_empty() {
        tally(engines::push(paths, client, lockfile, interactive, &changes.engines, relink, progress, env).await
            .with_context(|| format!("pushing engines for env '{env}'"))?);
    }
    if !changes.engine_fields.is_empty() {
        tally(engine_fields::push(paths, client, lockfile, interactive, &changes.engine_fields, progress, env).await
            .with_context(|| format!("pushing engine fields for env '{env}'"))?);
    }
    if !changes.schemas.is_empty() {
        tally(schemas::push(paths, client, lockfile, interactive, &changes.schemas, progress, env).await
            .with_context(|| format!("pushing schemas for env '{env}'"))?);
    }
    if !changes.queues.is_empty() {
        tally(queues::push(paths, client, lockfile, interactive, &changes.queues, relink, progress, env).await
            .with_context(|| format!("pushing queues for env '{env}'"))?);
    }
    if !changes.inboxes.is_empty() {
        tally(inboxes::push(paths, client, lockfile, interactive, &changes.inboxes, progress, env).await
            .with_context(|| format!("pushing inboxes for env '{env}'"))?);
    }
    if !changes.email_templates.is_empty() {
        tally(email_templates::push(paths, client, lockfile, interactive, &changes.email_templates, progress, env).await
            .with_context(|| format!("pushing email templates for env '{env}'"))?);
    }
    // Hooks always go through `push` (no early-skip on empty `changes`)
    // so the secrets-only pass inside it can detect changes to
    // `secrets/<env>.hook-secrets.json` that aren't accompanied by a
    // hook JSON/code edit. The function returns (0, 0) when neither
    // content nor secrets have drifted.
    tally(hooks::push(paths, client, lockfile, interactive, &changes.hooks, catalog_hooks, relink, progress, env)
        .await
        .with_context(|| format!("pushing hooks for env '{env}'"))?);
    // Labels before rules: a rule's `actions` can reference a label
    // (`rdc://labels/<slug>`), and `rules::push` resolves those refs at
    // create time. Labels are leaf objects (they reference only the
    // organization), so creating them first lets the rule's label refs
    // resolve against the lockfile instead of failing with "Invalid
    // hyperlink - No URL match".
    if !changes.labels.is_empty() {
        tally(labels::push(paths, client, lockfile, interactive, &changes.labels, progress, env).await
            .with_context(|| format!("pushing labels for env '{env}'"))?);
    }
    // A saved view's `queues_filter` can reference a queue, already pushed
    // above; unlike labels/rules it does not itself participate in another
    // kind's create-time ref resolution, so its ordering here is otherwise free.
    if !changes.saved_views.is_empty() {
        tally(saved_views::push(paths, client, lockfile, interactive, &changes.saved_views, progress, env).await
            .with_context(|| format!("pushing saved views for env '{env}'"))?);
    }
    if !changes.rules.is_empty() {
        tally(rules::push(paths, client, lockfile, interactive, &changes.rules, progress, env).await
            .with_context(|| format!("pushing rules for env '{env}'"))?);
    }
    // Last: the organization singleton references nothing and nothing
    // references it, so its ordering relative to every other kind is free.
    if let Some(path) = &changes.organization {
        tally(
            organization::push(paths, client, lockfile, path, progress, env)
                .await
                .with_context(|| format!("pushing organization for env '{env}'"))?,
        );
    }
    Ok((pushed, skipped))
}
