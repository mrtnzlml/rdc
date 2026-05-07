use super::common::{hash_for_lockfile, record_object, PullCtx};
use crate::slug::slugify_unique;
use crate::snapshot::hook::write_hook;
use anyhow::{Context, Result};
use std::collections::HashSet;

/// Pull all hooks from the env's remote into the local snapshot.
/// Returns the number of hooks pulled.
pub async fn pull(ctx: &mut PullCtx<'_>) -> Result<usize> {
    let hooks = ctx
        .client
        .list_hooks()
        .await
        .context("listing hooks")?;

    let mut used_slugs: HashSet<String> = HashSet::new();
    let mut dir_created = false;
    for hook in &hooks {
        if !dir_created {
            std::fs::create_dir_all(ctx.paths.hooks_dir())
                .with_context(|| format!("creating {}", ctx.paths.hooks_dir().display()))?;
            dir_created = true;
        }
        let slug = slugify_unique(&hook.name, &used_slugs);
        used_slugs.insert(slug.clone());

        let bytes = write_hook(&ctx.paths.hooks_dir(), &slug, hook)
            .with_context(|| format!("writing hook '{}' to disk", hook.name))?;
        let hash = hash_for_lockfile(&bytes);

        let modified_at = hook.modified_at().map(|s| s.to_string());

        record_object(
            ctx.lockfile,
            "hooks",
            &slug,
            hook.id,
            Some(hook.url.clone()),
            modified_at,
            Some(hash),
        );
    }

    Ok(hooks.len())
}
