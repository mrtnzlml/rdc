use super::common::{apply_pull_action, decide_pull_action, record_object, PullAction, PullCtx};
use crate::slug::slugify_unique;
use anyhow::{Context, Result};
use std::collections::HashSet;

/// Pull all engines. Returns `(count, conflicts)`.
pub async fn pull(ctx: &mut PullCtx<'_>) -> Result<(usize, usize)> {
    let engines = ctx.client.list_engines().await.context("listing engines")?;

    let mut used: HashSet<String> = HashSet::new();
    let mut dir_created = false;
    let mut conflicts = 0usize;
    for e in &engines {
        if !dir_created {
            std::fs::create_dir_all(ctx.paths.engines_dir())
                .with_context(|| format!("creating {}", ctx.paths.engines_dir().display()))?;
            dir_created = true;
        }
        let slug = match ctx.lockfile.slug_for_id("engines", e.id) {
            Some(existing) => existing.to_string(),
            None => slugify_unique(&e.name, &used),
        };
        used.insert(slug.clone());

        let mut proposed = serde_json::to_vec_pretty(e).context("serializing engine")?;
        proposed.push(b'\n');

        let local_path = ctx.paths.engines_dir().join(format!("{slug}.json"));
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
        let recorded_hash = apply_pull_action(action, &local_path, &proposed, remote_hash)?;

        record_object(
            ctx.lockfile,
            "engines",
            &slug,
            e.id,
            Some(e.url.clone()),
            e.modified_at().map(|s| s.to_string()),
            Some(recorded_hash),
        );
    }

    Ok((engines.len(), conflicts))
}
