use super::common::{apply_pull_action, decide_pull_action, record_object, skip_on_permission_denied, PullAction, PullCtx};
use crate::model::Workflow;
use crate::progress::SyncRenderer;
use crate::slug::slugify_unique;
use anyhow::{Context, Result};
use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

/// Phase 1: list all workflows from the API.
pub async fn list(ctx: &PullCtx<'_>, progress: &Arc<dyn SyncRenderer>) -> Result<Vec<Workflow>> {
    skip_on_permission_denied(
        ctx.client.list_workflows(Some(progress.clone())).await.context("listing workflows"),
        "workflows",
        progress,
    )
}

/// Phase 2: write listed workflows to disk. `subset` selects which `(kind,
/// slug)` pairs are written; items outside the subset are skipped silently.
/// Returns `(count, conflicts)` of items written.
pub async fn process(
    ctx: &mut PullCtx<'_>,
    workflows: Vec<Workflow>,
    subset: &BTreeSet<(String, String)>,
    progress: &Arc<dyn SyncRenderer>,
) -> Result<(usize, usize)> {
    progress.phase("pulling workflows");

    let mut used: HashSet<String> = HashSet::new();
    let mut conflicts = 0usize;
    let mut written = 0usize;
    for w in &workflows {
        let slug = match ctx.lockfile.slug_for_id("workflows", w.id) {
            Some(existing) => existing.to_string(),
            None => slugify_unique(&w.name, &used),
        };
        used.insert(slug.clone());

        if !subset.contains(&("workflows".to_string(), slug.clone())) {
            continue;
        }

        // Each workflow owns a directory: `workflows/<slug>/`. The
        // workflow's JSON lives at `workflow.json` inside it, alongside
        // `steps/`.
        let workflow_dir = ctx.paths.workflow_dir(&slug);
        std::fs::create_dir_all(&workflow_dir)
            .with_context(|| format!("creating {}", workflow_dir.display()))?;

        let mut proposed = serde_json::to_vec_pretty(w).context("serializing workflow")?;
        proposed.push(b'\n');

        let local_path = workflow_dir.join("workflow.json");
        let base_hash = ctx
            .lockfile
            .objects
            .get("workflows")
            .and_then(|m| m.get(&slug))
            .and_then(|x| x.content_hash.clone());

        let (action, remote_hash) =
            decide_pull_action(&local_path, base_hash.as_deref(), &proposed)?;
        if action == PullAction::Conflict {
            conflicts += 1;
        }
        let recorded_hash = apply_pull_action(action, &local_path, &proposed, remote_hash, ctx.interactive, progress, ctx.paths.env(), base_hash.as_deref())?;

        record_object(
            ctx.lockfile,
            "workflows",
            &slug,
            w.id,
            Some(w.url.clone()),
            w.modified_at().map(|s| s.to_string()),
            Some(recorded_hash),
        );
        written += 1;
    }

    if written > 0 {
        progress.warn_line(&format!("[ok] workflows {written} pulled"));
    }

    Ok((written, conflicts))
}
