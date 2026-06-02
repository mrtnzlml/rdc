use super::common::{PullAction, PullCtx, apply_pull_action, decide_pull_action, record_object};
use crate::log::{Action, Log};
use crate::model::Organization;
use anyhow::{Context, Result};
use std::sync::Arc;

const KIND: &str = "organization";

/// Phase 1: fetch the org singleton from the API.
pub async fn list(ctx: &PullCtx<'_>, org_id: u64, progress: &Arc<Log>) -> Result<Organization> {
    ctx.client
        .get_organization(org_id, Some(progress.clone()))
        .await
        .with_context(|| format!("fetching organization {org_id}"))
}

/// Phase 2: write the org to disk. Returns `(count, conflicts)`.
pub async fn process(
    ctx: &mut PullCtx<'_>,
    org: Organization,
    progress: &Arc<Log>,
) -> Result<(usize, usize)> {
    let result: Result<(usize, usize)> = (|| {
        let path = ctx.paths.organization_file();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }

        // Canonical on-disk bytes via KindCodec: strips `modified_at`.
        // No overlay for organization.
        let value = serde_json::to_value(&org)?;
        let art = crate::snapshot::codec::codec(KIND)
            .unwrap()
            .disk_bytes(&value)
            .context("serializing organization")?;
        let proposed = art.json;

        let base_hash = ctx
            .lockfile
            .objects
            .get(KIND)
            .and_then(|m| m.get("self"))
            .and_then(|e| e.content_hash.clone());

        let (action, remote_hash) = decide_pull_action(&path, base_hash.as_deref(), &proposed)?;
        let conflicts = if action == PullAction::Conflict { 1 } else { 0 };
        let recorded_hash = apply_pull_action(
            action,
            &path,
            &proposed,
            remote_hash,
            ctx.interactive,
            progress,
            ctx.paths.env(),
            base_hash.as_deref(),
            Some(ctx.paths),
        )?;

        record_object(
            ctx.lockfile,
            KIND,
            "self",
            org.id,
            Some(org.url.clone()),
            org.modified_at().map(|s| s.to_string()),
            Some(recorded_hash),
        );
        progress.event(Action::Pull, &format!("organization ({} pulled)", org.name));

        Ok((1, conflicts))
    })();
    result
}
