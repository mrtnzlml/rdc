use super::common::{apply_pull_action, decide_pull_action, record_object, skip_on_permission_denied, PullAction, PullCtx};
use crate::slug::slugify_unique;
use anyhow::{Context, Result};
use std::collections::HashSet;

/// Pull all labels. Returns `(count, conflicts)`.
pub async fn pull(ctx: &mut PullCtx<'_>) -> Result<(usize, usize)> {
    let labels = skip_on_permission_denied(
        ctx.client.list_labels().await.context("listing labels"),
        "labels",
    )?;

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
        let recorded_hash = apply_pull_action(action, &local_path, &proposed, remote_hash)?;

        record_object(
            ctx.lockfile,
            "labels",
            &slug,
            l.id,
            Some(l.url.clone()),
            l.modified_at().map(|s| s.to_string()),
            Some(recorded_hash),
        );
    }

    Ok((labels.len(), conflicts))
}
