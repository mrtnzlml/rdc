use super::common::{hash_for_lockfile, record_object, PullCtx};
use crate::slug::slugify_unique;
use crate::snapshot::engine::write_engine;
use anyhow::{Context, Result};
use std::collections::HashSet;

/// Pull all engines. Returns the count.
pub async fn pull(ctx: &mut PullCtx<'_>) -> Result<usize> {
    let engines = ctx.client.list_engines().await.context("listing engines")?;

    let mut used: HashSet<String> = HashSet::new();
    let mut dir_created = false;
    for e in &engines {
        if !dir_created {
            std::fs::create_dir_all(ctx.paths.engines_dir())
                .with_context(|| format!("creating {}", ctx.paths.engines_dir().display()))?;
            dir_created = true;
        }
        let slug = slugify_unique(&e.name, &used);
        used.insert(slug.clone());

        let bytes = write_engine(&ctx.paths.engines_dir(), &slug, e)
            .with_context(|| format!("writing engine '{}' to disk", e.name))?;
        let hash = hash_for_lockfile(&bytes);

        record_object(
            ctx.lockfile,
            "engines",
            &slug,
            e.id,
            Some(e.url.clone()),
            e.modified_at().map(|s| s.to_string()),
            Some(hash),
        );
    }

    Ok(engines.len())
}
