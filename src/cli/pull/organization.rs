use super::common::{apply_pull_action, decide_pull_action, record_object, PullAction, PullCtx};
use crate::model::Organization;
use crate::progress::OverallProgress;
use anyhow::{Context, Result};
use std::sync::Arc;

/// Phase 1: fetch the org singleton from the API.
pub async fn list(ctx: &PullCtx<'_>, org_id: u64, progress: &Arc<OverallProgress>) -> Result<Organization> {
    ctx.client
        .get_organization(org_id, Some(progress.clone()))
        .await
        .with_context(|| format!("fetching organization {org_id}"))
}

/// Phase 2: write the org to disk. Returns `(count, conflicts)`.
pub async fn process(ctx: &mut PullCtx<'_>, org: Organization, progress: &Arc<OverallProgress>) -> Result<(usize, usize)> {
    progress.start_phase("organization");

    let path = ctx.paths.organization_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let mut proposed = serde_json::to_vec_pretty(&org).context("serializing organization")?;
    proposed.push(b'\n');

    let base_hash = ctx
        .lockfile
        .objects
        .get("organization")
        .and_then(|m| m.get("self"))
        .and_then(|e| e.content_hash.clone());

    let (action, remote_hash) = decide_pull_action(&path, base_hash.as_deref(), &proposed)?;
    let conflicts = if action == PullAction::Conflict { 1 } else { 0 };
    let recorded_hash = apply_pull_action(action, &path, &proposed, remote_hash, ctx.interactive, progress, &ctx.env)?;

    record_object(
        ctx.lockfile,
        "organization",
        "self",
        org.id,
        Some(org.url.clone()),
        org.modified_at().map(|s| s.to_string()),
        Some(recorded_hash),
    );
    progress.tick(&org.name);

    Ok((1, conflicts))
}
