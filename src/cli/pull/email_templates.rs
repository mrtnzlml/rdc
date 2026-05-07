use super::common::{hash_for_lockfile, record_object, PullCtx};
use crate::slug::slugify_unique;
use crate::snapshot::email_template::write_email_template;
use anyhow::{Context, Result};
use std::collections::HashSet;

/// Pull all email templates. Returns the count.
pub async fn pull(ctx: &mut PullCtx<'_>) -> Result<usize> {
    let templates = ctx.client.list_email_templates().await.context("listing email templates")?;

    let mut used: HashSet<String> = HashSet::new();
    let mut dir_created = false;
    for t in &templates {
        if !dir_created {
            std::fs::create_dir_all(ctx.paths.email_templates_dir())
                .with_context(|| format!("creating {}", ctx.paths.email_templates_dir().display()))?;
            dir_created = true;
        }
        let slug = slugify_unique(&t.name, &used);
        used.insert(slug.clone());

        let bytes = write_email_template(&ctx.paths.email_templates_dir(), &slug, t)
            .with_context(|| format!("writing email template '{}' to disk", t.name))?;
        let hash = hash_for_lockfile(&bytes);

        record_object(
            ctx.lockfile,
            "email_templates",
            &slug,
            t.id,
            Some(t.url.clone()),
            t.modified_at().map(|s| s.to_string()),
            Some(hash),
        );
    }

    Ok(templates.len())
}
