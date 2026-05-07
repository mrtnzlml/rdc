use super::common::{apply_pull_action, decide_pull_action, record_object, PullAction, PullCtx};
use crate::slug::slugify_unique;
use anyhow::{Context, Result};
use std::collections::HashSet;

/// Pull all rules. Returns `(count, conflicts)`.
pub async fn pull(ctx: &mut PullCtx<'_>) -> Result<(usize, usize)> {
    let rules = ctx.client.list_rules().await.context("listing rules")?;

    let mut used: HashSet<String> = HashSet::new();
    let mut dir_created = false;
    let mut conflicts = 0usize;
    for r in &rules {
        if !dir_created {
            std::fs::create_dir_all(ctx.paths.rules_dir())
                .with_context(|| format!("creating {}", ctx.paths.rules_dir().display()))?;
            dir_created = true;
        }
        let slug = match ctx.lockfile.slug_for_id("rules", r.id) {
            Some(existing) => existing.to_string(),
            None => slugify_unique(&r.name, &used),
        };
        used.insert(slug.clone());

        let mut proposed = serde_json::to_vec_pretty(r).context("serializing rule")?;
        proposed.push(b'\n');

        let local_path = ctx.paths.rules_dir().join(format!("{slug}.json"));
        let base_hash = ctx
            .lockfile
            .objects
            .get("rules")
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
            "rules",
            &slug,
            r.id,
            Some(r.url.clone()),
            r.modified_at().map(|s| s.to_string()),
            Some(recorded_hash),
        );
    }

    Ok((rules.len(), conflicts))
}
