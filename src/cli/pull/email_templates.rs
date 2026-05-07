use super::common::{apply_pull_action, decide_pull_action, record_object, PullAction, PullCtx};
use crate::slug::slugify_unique;
use anyhow::{Context, Result};
use std::collections::HashSet;

/// Pull all email templates. Returns `(count, conflicts)`.
pub async fn pull(ctx: &mut PullCtx<'_>) -> Result<(usize, usize)> {
    let templates = ctx.client.list_email_templates().await.context("listing email templates")?;

    let mut used: HashSet<String> = HashSet::new();
    let mut dir_created = false;
    let mut conflicts = 0usize;
    for t in &templates {
        if !dir_created {
            std::fs::create_dir_all(ctx.paths.email_templates_dir())
                .with_context(|| format!("creating {}", ctx.paths.email_templates_dir().display()))?;
            dir_created = true;
        }
        let slug = slugify_unique(&t.name, &used);
        used.insert(slug.clone());

        let mut proposed = serde_json::to_vec_pretty(t).context("serializing email template")?;
        proposed.push(b'\n');

        let local_path = ctx.paths.email_templates_dir().join(format!("{slug}.json"));
        let base_hash = ctx
            .lockfile
            .objects
            .get("email_templates")
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
            "email_templates",
            &slug,
            t.id,
            Some(t.url.clone()),
            t.modified_at().map(|s| s.to_string()),
            Some(recorded_hash),
        );
    }

    Ok((templates.len(), conflicts))
}
