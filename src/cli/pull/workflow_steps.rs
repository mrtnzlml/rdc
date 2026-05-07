use super::common::{hash_for_lockfile, record_object, PullCtx};
use crate::slug::slugify_unique;
use crate::snapshot::workflow_step::write_workflow_step;
use anyhow::{Context, Result};
use std::collections::HashSet;

/// Pull all workflow steps. Returns the count.
pub async fn pull(ctx: &mut PullCtx<'_>) -> Result<usize> {
    let steps = ctx.client.list_workflow_steps().await.context("listing workflow steps")?;

    let mut used: HashSet<String> = HashSet::new();
    let mut dir_created = false;
    for s in &steps {
        if !dir_created {
            std::fs::create_dir_all(ctx.paths.workflow_steps_dir())
                .with_context(|| format!("creating {}", ctx.paths.workflow_steps_dir().display()))?;
            dir_created = true;
        }
        let slug = slugify_unique(&s.name, &used);
        used.insert(slug.clone());

        let bytes = write_workflow_step(&ctx.paths.workflow_steps_dir(), &slug, s)
            .with_context(|| format!("writing workflow step '{}' to disk", s.name))?;
        let hash = hash_for_lockfile(&bytes);

        record_object(
            ctx.lockfile,
            "workflow_steps",
            &slug,
            s.id,
            Some(s.url.clone()),
            s.modified_at().map(|x| x.to_string()),
            Some(hash),
        );
    }

    Ok(steps.len())
}
