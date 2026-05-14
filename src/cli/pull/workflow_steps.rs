use super::common::{apply_pull_action, decide_pull_action, record_object, skip_on_permission_denied, PullAction, PullCtx};
use crate::model::WorkflowStep;
use crate::progress::OverallProgress;
use crate::slug::slugify_unique;
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::sync::Arc;

/// Phase 1: list all workflow steps from the API.
pub async fn list(ctx: &PullCtx<'_>, progress: &Arc<OverallProgress>) -> Result<Vec<WorkflowStep>> {
    skip_on_permission_denied(
        ctx.client.list_workflow_steps(Some(progress.clone())).await.context("listing workflow steps"),
        "workflow_steps",
        progress,
    )
}

/// Phase 2: write listed workflow steps to disk. Each step nests under
/// its parent workflow at `workflows/<workflow_slug>/steps/<step_slug>.json`.
/// Orphan steps (no workflow in the lockfile) are skipped with a warning.
pub async fn process(ctx: &mut PullCtx<'_>, steps: Vec<WorkflowStep>, progress: &Arc<OverallProgress>) -> Result<(usize, usize)> {
    progress.start_phase("workflow_steps");

    let mut used: HashSet<String> = HashSet::new();
    let mut conflicts = 0usize;
    let mut written = 0usize;
    for s in &steps {
        let Some(workflow_slug) = ctx.lockfile.slug_for_url("workflows", &s.workflow).map(|x| x.to_string()) else {
            progress.skipped_orphan();
            progress.println(format!(
                "warning: workflow step '{}' (id {}) has unknown workflow URL '{}'; skipping",
                s.name, s.id, s.workflow
            ));
            continue;
        };

        let slug = match ctx.lockfile.slug_for_id("workflow_steps", s.id) {
            Some(existing) => existing.to_string(),
            None => slugify_unique(&s.name, &used),
        };
        used.insert(slug.clone());

        let steps_dir = ctx.paths.workflow_steps_dir(&workflow_slug);
        std::fs::create_dir_all(&steps_dir)
            .with_context(|| format!("creating {}", steps_dir.display()))?;

        let mut proposed = serde_json::to_vec_pretty(s).context("serializing workflow step")?;
        proposed.push(b'\n');

        let local_path = steps_dir.join(format!("{slug}.json"));
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
        let recorded_hash = apply_pull_action(action, &local_path, &proposed, remote_hash, ctx.interactive, progress, ctx.paths.env())?;

        record_object(
            ctx.lockfile,
            "workflow_steps",
            &slug,
            s.id,
            Some(s.url.clone()),
            s.modified_at().map(|x| x.to_string()),
            Some(recorded_hash),
        );
        progress.tick(&s.name);
        written += 1;
    }

    Ok((written, conflicts))
}
