use super::common::{apply_pull_action, decide_pull_action, record_object, PullAction, PullCtx};
use crate::slug::slugify_unique;
use crate::snapshot::hook::write_hook_code;
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashSet;

/// Pull all hooks. Returns `(count, conflicts)`.
pub async fn pull(ctx: &mut PullCtx<'_>) -> Result<(usize, usize)> {
    let hooks = ctx.client.list_hooks().await.context("listing hooks")?;

    let mut used_slugs: HashSet<String> = HashSet::new();
    let mut dir_created = false;
    let mut conflicts = 0usize;
    for hook in &hooks {
        if !dir_created {
            std::fs::create_dir_all(ctx.paths.hooks_dir())
                .with_context(|| format!("creating {}", ctx.paths.hooks_dir().display()))?;
            dir_created = true;
        }
        let slug = slugify_unique(&hook.name, &used_slugs);
        used_slugs.insert(slug.clone());

        // Build the JSON body the same way the codec would (without `code`).
        let mut json_value = serde_json::to_value(hook).context("serializing hook")?;
        let code = json_value
            .get_mut("config")
            .and_then(|c| c.as_object_mut())
            .and_then(|m| m.remove("code"))
            .and_then(|v| match v {
                Value::String(s) => Some(s),
                _ => None,
            });
        let mut proposed = serde_json::to_vec_pretty(&json_value)
            .context("serializing hook json")?;
        proposed.push(b'\n');

        let local_path = ctx.paths.hooks_dir().join(format!("{slug}.json"));
        let base_hash = ctx
            .lockfile
            .objects
            .get("hooks")
            .and_then(|m| m.get(&slug))
            .and_then(|e| e.content_hash.clone());

        let (action, remote_hash) =
            decide_pull_action(&local_path, base_hash.as_deref(), &proposed)?;
        if action == PullAction::Conflict {
            conflicts += 1;
        }
        let recorded_hash = apply_pull_action(action, &local_path, &proposed, remote_hash)?;

        // Hook code (.py) is always overwritten — out of M7 three-way scope.
        if let Some(code) = code {
            write_hook_code(&ctx.paths.hooks_dir(), &slug, &code)
                .with_context(|| format!("writing hook code for '{}'", hook.name))?;
        }

        record_object(
            ctx.lockfile,
            "hooks",
            &slug,
            hook.id,
            Some(hook.url.clone()),
            hook.modified_at().map(|s| s.to_string()),
            Some(recorded_hash),
        );
    }

    Ok((hooks.len(), conflicts))
}
