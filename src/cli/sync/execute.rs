//! Execute the classified plan. Today this dispatches pull-side writes
//! (`RemoteEdit`, `RemoteCreate`) and push-side writes (`LocalEdit`,
//! `LocalCreate`) by delegating to the existing per-kind pull / push
//! pipelines with a subset filter. The both-diverged resolver and the
//! remote-delete / double-conflict paths land in subsequent tasks.
//!
//! Spec: docs/superpowers/specs/2026-05-14-unified-sync-design.md.

use crate::cli::sync::classify::{ClassifiedItem, SyncClass};
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};

/// Dispatch the classified items. Pull-side items (`RemoteEdit` /
/// `RemoteCreate`) are grouped by kind and the per-kind pull driver
/// runs once per kind with a `(kind, slug)` subset filter. Push-side
/// items (`LocalEdit` / `LocalCreate`) are folded into a `ChangeList`
/// via [`crate::cli::push::scan::change_list_from_classified`] and
/// handed off to [`crate::cli::push::push_classified`], the same entry
/// point that `rdc push` uses.
///
/// `interactive` is unused on the pull side (the per-driver
/// `apply_pull_action` consults `ctx.interactive`); the push side reads
/// it explicitly for the drift-resolver prompt.
pub async fn run(
    ctx: &mut crate::cli::pull::common::PullCtx<'_>,
    catalog: &crate::cli::pull::common::RemoteCatalog,
    classified: &[ClassifiedItem],
    no_push: bool,
    no_pull: bool,
    interactive: bool,
    progress: &std::sync::Arc<crate::progress::OverallProgress>,
) -> Result<()> {
    if !no_pull {
        // Group pull-side items by kind so each driver runs at most
        // once per sync. Slugs inside the subset filter through the
        // driver's own `subset.contains(...)` guard.
        let mut subsets: BTreeMap<&str, BTreeSet<(String, String)>> = BTreeMap::new();
        for it in classified {
            if matches!(it.class, SyncClass::RemoteEdit | SyncClass::RemoteCreate) {
                subsets
                    .entry(it.kind.as_str())
                    .or_default()
                    .insert((it.kind.clone(), it.slug.clone()));
            }
        }

        // labels: flat slug, no nested files. Wired today because Task
        // 14 ships the labels-only adapter; other kinds plug in as
        // their adapter coverage lands.
        if let Some(subset) = subsets.get("labels") {
            crate::cli::pull::labels::process(ctx, catalog.labels.clone(), subset, progress).await?;
        }

        // TODO(sync-impl): add per-kind dispatch as their adapter
        // hashing arrives — workspaces, queues, schemas, inboxes,
        // hooks, rules, engines, engine_fields, email_templates,
        // workflows, workflow_steps, mdh. Each kind's `process` already
        // accepts a subset filter; the only new code is the
        // `subsets.get("<kind>") → process(...)` line plus any
        // catalog-side prerequisites (e.g. queues needs the workspace
        // map populated; see `pull::run_drivers` for ordering).
    }

    if !no_push {
        // Fold LocalEdit / LocalCreate items into the same `ChangeList`
        // shape `push::scan::scan` produces, then delegate to the
        // existing push pipeline. `push_classified` mirrors what
        // `rdc push <env>` runs after its own scan/dry-run gate, so
        // sync inherits drift detection, conflict prompts, and progress
        // ticking for free.
        let change_list =
            crate::cli::push::scan::change_list_from_classified(ctx.paths, classified);
        if !change_list.is_empty() {
            let env = ctx.paths.env().to_string();
            crate::cli::push::push_classified(
                ctx.paths,
                ctx.client,
                ctx.lockfile,
                &env,
                interactive,
                &change_list,
                progress,
            )
            .await?;
        }
    }

    // Conflict + remote-delete branches: Tasks 16, 17.
    Ok(())
}
