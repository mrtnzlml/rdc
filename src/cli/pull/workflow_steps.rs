use super::common::{apply_pull_action, decide_pull_action, record_object, skip_on_permission_denied, PullAction, PullCtx};
use crate::progress::KindProgress;
use crate::slug::slugify_unique;
use anyhow::{Context, Result};
use std::collections::HashSet;

/// Pull all workflow steps. Returns `(count, conflicts)`.
pub async fn pull(ctx: &mut PullCtx<'_>, progress: &KindProgress) -> Result<(usize, usize)> {
    let steps = skip_on_permission_denied(
        ctx.client.list_workflow_steps().await.context("listing workflow steps"),
        "workflow_steps",
    )?;
    progress.set_total(steps.len() as u64);

    let mut used: HashSet<String> = HashSet::new();
    let mut dir_created = false;
    let mut conflicts = 0usize;
    for s in &steps {
        if !dir_created {
            std::fs::create_dir_all(ctx.paths.workflow_steps_dir())
                .with_context(|| format!("creating {}", ctx.paths.workflow_steps_dir().display()))?;
            dir_created = true;
        }
        let slug = match ctx.lockfile.slug_for_id("workflow_steps", s.id) {
            Some(existing) => existing.to_string(),
            None => slugify_unique(&s.name, &used),
        };
        used.insert(slug.clone());

        let mut proposed = serde_json::to_vec_pretty(s).context("serializing workflow step")?;
        proposed.push(b'\n');

        let local_path = ctx.paths.workflow_steps_dir().join(format!("{slug}.json"));
        let base_hash = ctx
            .lockfile
            .objects
            .get("workflow_steps")
            .and_then(|m| m.get(&slug))
            .and_then(|x| x.content_hash.clone());

        let (action, remote_hash) =
            decide_pull_action(&local_path, base_hash.as_deref(), &proposed)?;
        if action == PullAction::Conflict {
            conflicts += 1;
        }
        let recorded_hash = apply_pull_action(action, &local_path, &proposed, remote_hash, ctx.interactive)?;

        record_object(
            ctx.lockfile,
            "workflow_steps",
            &slug,
            s.id,
            Some(s.url.clone()),
            s.modified_at().map(|x| x.to_string()),
            Some(recorded_hash),
        );
        progress.tick();
    }

    Ok((steps.len(), conflicts))
}
