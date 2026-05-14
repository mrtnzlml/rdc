use super::common::{
    apply_pull_action, decide_pull_action, maybe_strip_overlay, record_object,
    skip_on_permission_denied, PullAction, PullCtx,
};
use crate::model::Engine;
use crate::progress::OverallProgress;
use crate::slug::slugify_unique;
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::sync::Arc;

/// Phase 1: list all engines from the API.
pub async fn list(ctx: &PullCtx<'_>, progress: &Arc<OverallProgress>) -> Result<Vec<Engine>> {
    skip_on_permission_denied(
        ctx.client.list_engines(Some(progress.clone())).await.context("listing engines"),
        "engines",
        progress,
    )
}

/// Phase 2: write listed engines to disk. Returns `(count, conflicts)`.
pub async fn process(ctx: &mut PullCtx<'_>, engines: Vec<Engine>, progress: &Arc<OverallProgress>) -> Result<(usize, usize)> {
    progress.start_phase("engines");

    let mut used: HashSet<String> = HashSet::new();
    let mut conflicts = 0usize;
    for e in &engines {
        let slug = match ctx.lockfile.slug_for_id("engines", e.id) {
            Some(existing) => existing.to_string(),
            None => slugify_unique(&e.name, &used),
        };
        used.insert(slug.clone());

        // Each engine owns a directory: `engines/<slug>/`. The engine's
        // JSON lives at `engine.json` inside it, alongside `fields/`.
        let engine_dir = ctx.paths.engine_dir(&slug);
        std::fs::create_dir_all(&engine_dir)
            .with_context(|| format!("creating {}", engine_dir.display()))?;

        let mut proposed = serde_json::to_vec_pretty(e).context("serializing engine")?;
        proposed.push(b'\n');
        let proposed = maybe_strip_overlay(
            proposed,
            ctx.overlay.as_ref().and_then(|o| o.engine(&slug)),
        )?;

        let local_path = engine_dir.join("engine.json");
        let base_hash = ctx
            .lockfile
            .objects
            .get("engines")
            .and_then(|m| m.get(&slug))
            .and_then(|x| x.content_hash.clone());

        let (action, remote_hash) =
            decide_pull_action(&local_path, base_hash.as_deref(), &proposed)?;
        if action == PullAction::Conflict {
            conflicts += 1;
        }
        let recorded_hash = apply_pull_action(action, &local_path, &proposed, remote_hash, ctx.interactive, progress, &ctx.env)?;

        record_object(
            ctx.lockfile,
            "engines",
            &slug,
            e.id,
            Some(e.url.clone()),
            e.modified_at().map(|s| s.to_string()),
            Some(recorded_hash),
        );
        progress.tick(&e.name);
    }

    Ok((engines.len(), conflicts))
}
