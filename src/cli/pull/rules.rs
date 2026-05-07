use super::common::{hash_for_lockfile, record_object, PullCtx};
use crate::slug::slugify_unique;
use crate::snapshot::rule::write_rule;
use anyhow::{Context, Result};
use std::collections::HashSet;

/// Pull all rules. Returns the count.
pub async fn pull(ctx: &mut PullCtx<'_>) -> Result<usize> {
    let rules = ctx.client.list_rules().await.context("listing rules")?;

    let mut used: HashSet<String> = HashSet::new();
    let mut dir_created = false;
    for r in &rules {
        if !dir_created {
            std::fs::create_dir_all(ctx.paths.rules_dir())
                .with_context(|| format!("creating {}", ctx.paths.rules_dir().display()))?;
            dir_created = true;
        }
        let slug = slugify_unique(&r.name, &used);
        used.insert(slug.clone());

        let bytes = write_rule(&ctx.paths.rules_dir(), &slug, r)
            .with_context(|| format!("writing rule '{}' to disk", r.name))?;
        let hash = hash_for_lockfile(&bytes);

        record_object(
            ctx.lockfile,
            "rules",
            &slug,
            r.id,
            Some(r.url.clone()),
            r.modified_at().map(|s| s.to_string()),
            Some(hash),
        );
    }

    Ok(rules.len())
}
