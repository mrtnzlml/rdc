use super::common::{
    apply_pull_action, decide_pull_action, maybe_strip_overlay, record_object,
    skip_on_permission_denied, PullAction, PullCtx,
};
use crate::model::EngineField;
use crate::progress::OverallProgress;
use crate::slug::slugify_unique;
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::sync::Arc;

/// Phase 1: list all engine fields from the API.
pub async fn list(ctx: &PullCtx<'_>, progress: &Arc<OverallProgress>) -> Result<Vec<EngineField>> {
    skip_on_permission_denied(
        ctx.client.list_engine_fields(Some(progress.clone())).await.context("listing engine fields"),
        "engine_fields",
        progress,
    )
}

/// Phase 2: write listed engine fields to disk. Each field nests under
/// its parent engine at `engines/<engine_slug>/fields/<field_slug>.json`.
/// Orphan fields (no engine in the lockfile) are skipped with a warning
/// — same pattern as orphan queues.
pub async fn process(ctx: &mut PullCtx<'_>, fields: Vec<EngineField>, progress: &Arc<OverallProgress>) -> Result<(usize, usize)> {
    progress.start_phase("engine_fields");

    let mut used: HashSet<String> = HashSet::new();
    let mut conflicts = 0usize;
    let mut written = 0usize;
    for f in &fields {
        let Some(engine_slug) = ctx.lockfile.slug_for_url("engines", &f.engine).map(|s| s.to_string()) else {
            progress.skipped_orphan();
            progress.println(format!(
                "warning: engine field '{}' (id {}) has unknown engine URL '{}'; skipping",
                f.name, f.id, f.engine
            ));
            continue;
        };

        let slug = match ctx.lockfile.slug_for_id("engine_fields", f.id) {
            Some(existing) => existing.to_string(),
            None => slugify_unique(&f.name, &used),
        };
        used.insert(slug.clone());

        let fields_dir = ctx.paths.engine_fields_dir(&engine_slug);
        std::fs::create_dir_all(&fields_dir)
            .with_context(|| format!("creating {}", fields_dir.display()))?;

        let mut proposed = serde_json::to_vec_pretty(f).context("serializing engine field")?;
        proposed.push(b'\n');
        let proposed = maybe_strip_overlay(
            proposed,
            ctx.overlay.as_ref().and_then(|o| o.engine_field(&slug)),
        )?;

        let local_path = fields_dir.join(format!("{slug}.json"));
        let base_hash = ctx
            .lockfile
            .objects
            .get("engine_fields")
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
            "engine_fields",
            &slug,
            f.id,
            Some(f.url.clone()),
            f.modified_at().map(|s| s.to_string()),
            Some(recorded_hash),
        );
        progress.tick(&f.name);
        written += 1;
    }

    Ok((written, conflicts))
}
