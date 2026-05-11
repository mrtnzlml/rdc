use super::common::{hash_for_lockfile, record_object, skip_on_permission_denied, PullCtx};
use crate::model::Workspace;
use crate::progress::OverallProgress;
use crate::slug::slugify_unique;
use crate::snapshot::workspace::write_workspace;
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::sync::Arc;

/// Phase 1: list all workspaces from the API.
pub async fn list(ctx: &PullCtx<'_>, progress: &Arc<OverallProgress>) -> Result<Vec<Workspace>> {
    skip_on_permission_denied(
        ctx.client.list_workspaces(Some(progress.clone())).await.context("listing workspaces"),
        "workspaces",
        progress,
    )
}

/// Phase 2: write listed workspaces to disk. Returns the number written.
pub async fn process(ctx: &mut PullCtx<'_>, workspaces: Vec<Workspace>, progress: &Arc<OverallProgress>) -> Result<usize> {
    progress.start_phase("workspaces");

    let mut used_slugs: HashSet<String> = HashSet::new();
    let mut dir_created = false;
    let mut count = 0usize;
    for ws in &workspaces {
        if !dir_created {
            std::fs::create_dir_all(ctx.paths.workspaces_dir())
                .with_context(|| format!("creating {}", ctx.paths.workspaces_dir().display()))?;
            dir_created = true;
        }

        let slug = match ctx.lockfile.slug_for_id("workspaces", ws.id) {
            Some(existing) => existing.to_string(),
            None => slugify_unique(&ws.name, &used_slugs),
        };
        used_slugs.insert(slug.clone());

        let ws_dir = ctx.paths.workspace_dir(&slug);
        std::fs::create_dir_all(&ws_dir)
            .with_context(|| format!("creating {}", ws_dir.display()))?;

        let bytes = write_workspace(&ws_dir, ws)
            .with_context(|| format!("writing workspace '{}' to disk", ws.name))?;
        let hash = hash_for_lockfile(&bytes);

        record_object(
            ctx.lockfile,
            "workspaces",
            &slug,
            ws.id,
            Some(ws.url.clone()),
            ws.modified_at().map(|s| s.to_string()),
            Some(hash),
        );

        count += 1;
        progress.tick(&ws.name);
    }

    Ok(count)
}
