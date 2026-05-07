use super::common::{hash_for_lockfile, record_object, PullCtx};
use crate::slug::slugify_unique;
use crate::snapshot::workflow::write_workflow;
use anyhow::{Context, Result};
use std::collections::HashSet;

/// Pull all workflows. Returns the count.
pub async fn pull(ctx: &mut PullCtx<'_>) -> Result<usize> {
    let workflows = ctx.client.list_workflows().await.context("listing workflows")?;

    let mut used: HashSet<String> = HashSet::new();
    let mut dir_created = false;
    for w in &workflows {
        if !dir_created {
            std::fs::create_dir_all(ctx.paths.workflows_dir())
                .with_context(|| format!("creating {}", ctx.paths.workflows_dir().display()))?;
            dir_created = true;
        }
        let slug = slugify_unique(&w.name, &used);
        used.insert(slug.clone());

        let bytes = write_workflow(&ctx.paths.workflows_dir(), &slug, w)
            .with_context(|| format!("writing workflow '{}' to disk", w.name))?;
        let hash = hash_for_lockfile(&bytes);

        record_object(
            ctx.lockfile,
            "workflows",
            &slug,
            w.id,
            Some(w.url.clone()),
            w.modified_at().map(|s| s.to_string()),
            Some(hash),
        );
    }

    Ok(workflows.len())
}
