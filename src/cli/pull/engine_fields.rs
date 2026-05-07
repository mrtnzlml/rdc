use super::common::{hash_for_lockfile, record_object, PullCtx};
use crate::slug::slugify_unique;
use crate::snapshot::engine_field::write_engine_field;
use anyhow::{Context, Result};
use std::collections::HashSet;

/// Pull all engine fields. Returns the count.
pub async fn pull(ctx: &mut PullCtx<'_>) -> Result<usize> {
    let fields = ctx
        .client
        .list_engine_fields()
        .await
        .context("listing engine fields")?;

    let mut used: HashSet<String> = HashSet::new();
    let mut dir_created = false;
    for f in &fields {
        if !dir_created {
            std::fs::create_dir_all(ctx.paths.engine_fields_dir())
                .with_context(|| format!("creating {}", ctx.paths.engine_fields_dir().display()))?;
            dir_created = true;
        }
        let slug = slugify_unique(&f.name, &used);
        used.insert(slug.clone());

        let bytes = write_engine_field(&ctx.paths.engine_fields_dir(), &slug, f)
            .with_context(|| format!("writing engine field '{}' to disk", f.name))?;
        let hash = hash_for_lockfile(&bytes);

        record_object(
            ctx.lockfile,
            "engine_fields",
            &slug,
            f.id,
            Some(f.url.clone()),
            f.modified_at().map(|s| s.to_string()),
            Some(hash),
        );
    }

    Ok(fields.len())
}
