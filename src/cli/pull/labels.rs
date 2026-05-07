use super::common::{
    apply_pull_action, decide_pull_action, maybe_strip_overlay, record_object,
    skip_on_permission_denied, PullAction, PullCtx,
};
use crate::model::Label;
use crate::progress::OverallProgress;
use crate::slug::slugify_unique;
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::sync::Arc;

/// Phase 1: list all labels from the API.
pub async fn list(ctx: &PullCtx<'_>, progress: &Arc<OverallProgress>) -> Result<Vec<Label>> {
    skip_on_permission_denied(
        ctx.client.list_labels(Some(progress.clone())).await.context("listing labels"),
        "labels",
        progress,
    )
}

/// Phase 2: write listed labels to disk. Returns `(count, conflicts)`.
pub async fn process(ctx: &mut PullCtx<'_>, labels: Vec<Label>, progress: &Arc<OverallProgress>) -> Result<(usize, usize)> {
    progress.start_phase("labels");

    let mut used: HashSet<String> = HashSet::new();
    let mut dir_created = false;
    let mut conflicts = 0usize;
    for l in &labels {
        if !dir_created {
            std::fs::create_dir_all(ctx.paths.labels_dir())
                .with_context(|| format!("creating {}", ctx.paths.labels_dir().display()))?;
            dir_created = true;
        }
        let slug = match ctx.lockfile.slug_for_id("labels", l.id) {
            Some(existing) => existing.to_string(),
            None => slugify_unique(&l.name, &used),
        };
        used.insert(slug.clone());

        let mut proposed = serde_json::to_vec_pretty(l).context("serializing label")?;
        proposed.push(b'\n');
        let proposed = maybe_strip_overlay(
            proposed,
            ctx.overlay.as_ref().and_then(|o| o.label(&slug)),
        )?;

        let local_path = ctx.paths.labels_dir().join(format!("{slug}.json"));
        let base_hash = ctx
            .lockfile
            .objects
            .get("labels")
            .and_then(|m| m.get(&slug))
            .and_then(|e| e.content_hash.clone());

        let (action, remote_hash) =
            decide_pull_action(&local_path, base_hash.as_deref(), &proposed)?;
        if action == PullAction::Conflict {
            conflicts += 1;
        }
        let recorded_hash = apply_pull_action(action, &local_path, &proposed, remote_hash, ctx.interactive, progress)?;

        record_object(
            ctx.lockfile,
            "labels",
            &slug,
            l.id,
            Some(l.url.clone()),
            l.modified_at().map(|s| s.to_string()),
            Some(recorded_hash),
        );
        progress.tick(&l.name);
    }

    Ok((labels.len(), conflicts))
}
