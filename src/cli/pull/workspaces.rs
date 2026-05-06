use super::common::{hash_for_lockfile, record_object, PullCtx};
use crate::slug::slugify_unique;
use crate::snapshot::workspace::write_workspace;
use anyhow::{Context, Result};
use std::collections::HashSet;

/// Pull all workspaces from the env's remote. Each workspace is written as
/// `envs/<env>/workspaces/<slug>/workspace.json`.
/// Returns the number of workspaces pulled.
pub async fn pull(ctx: &mut PullCtx<'_>) -> Result<usize> {
    let workspaces = ctx
        .client
        .list_workspaces()
        .await
        .context("listing workspaces")?;

    std::fs::create_dir_all(ctx.paths.workspaces_dir())
        .with_context(|| format!("creating {}", ctx.paths.workspaces_dir().display()))?;

    let mut used_slugs: HashSet<String> = HashSet::new();
    for ws in &workspaces {
        let slug = slugify_unique(&ws.name, &used_slugs);
        used_slugs.insert(slug.clone());

        let ws_dir = ctx.paths.workspace_dir(&slug);
        std::fs::create_dir_all(&ws_dir)
            .with_context(|| format!("creating {}", ws_dir.display()))?;

        write_workspace(&ws_dir, ws)
            .with_context(|| format!("writing workspace '{}' to disk", ws.name))?;

        let json_path = ws_dir.join("workspace.json");
        let bytes = std::fs::read(&json_path)
            .with_context(|| format!("reading just-written {}", json_path.display()))?;
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
    }

    Ok(workspaces.len())
}
