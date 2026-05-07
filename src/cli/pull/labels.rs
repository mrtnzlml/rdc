use super::common::{hash_for_lockfile, record_object, PullCtx};
use crate::slug::slugify_unique;
use crate::snapshot::label::write_label;
use anyhow::{Context, Result};
use std::collections::HashSet;

/// Pull all labels. Returns the count.
pub async fn pull(ctx: &mut PullCtx<'_>) -> Result<usize> {
    let labels = ctx.client.list_labels().await.context("listing labels")?;

    let mut used: HashSet<String> = HashSet::new();
    let mut dir_created = false;
    for l in &labels {
        if !dir_created {
            std::fs::create_dir_all(ctx.paths.labels_dir())
                .with_context(|| format!("creating {}", ctx.paths.labels_dir().display()))?;
            dir_created = true;
        }
        let slug = slugify_unique(&l.name, &used);
        used.insert(slug.clone());

        let bytes = write_label(&ctx.paths.labels_dir(), &slug, l)
            .with_context(|| format!("writing label '{}' to disk", l.name))?;
        let hash = hash_for_lockfile(&bytes);

        record_object(
            ctx.lockfile,
            "labels",
            &slug,
            l.id,
            Some(l.url.clone()),
            l.modified_at().map(|s| s.to_string()),
            Some(hash),
        );
    }

    Ok(labels.len())
}
