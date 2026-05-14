//! Execute the classified plan. Today this dispatches pull-side writes
//! (`RemoteEdit`, `RemoteCreate`) by delegating to the per-kind
//! `cli::pull::<kind>::process` driver with a subset filter. Push-side
//! writes, the both-diverged resolver, and the remote-delete /
//! double-conflict paths land in subsequent tasks.
//!
//! Spec: docs/superpowers/specs/2026-05-14-unified-sync-design.md.

use crate::cli::sync::classify::{ClassifiedItem, SyncClass};
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};

/// Dispatch the classified items. Pull-side items (`RemoteEdit` /
/// `RemoteCreate`) are grouped by kind, and the per-kind pull driver
/// runs once per kind with a `(kind, slug)` subset filter. Drivers
/// silently skip items outside the subset, so only the classified
/// slugs are written.
///
/// `no_push` is currently unused — push-side dispatch lands in Task 15.
/// `interactive` is unused here; the conflict resolver lives in the
/// per-driver code path (`apply_pull_action`) which already consults
/// `ctx.interactive`.
pub async fn run(
    ctx: &mut crate::cli::pull::common::PullCtx<'_>,
    catalog: &crate::cli::pull::common::RemoteCatalog,
    classified: &[ClassifiedItem],
    no_push: bool,
    no_pull: bool,
    interactive: bool,
    progress: &std::sync::Arc<crate::progress::OverallProgress>,
) -> Result<()> {
    // `no_push` and `interactive` flow into Tasks 15-17. Mark them
    // explicitly consumed so the compiler treats today's no-op as
    // intentional rather than a forgotten parameter.
    let _ = (no_push, interactive);

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

    // Push-side, conflict, remote-delete branches: Tasks 15, 16, 17.
    Ok(())
}
