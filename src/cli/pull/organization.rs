use super::common::{hash_for_lockfile, record_object, PullCtx};
use crate::snapshot::organization::write_organization;
use anyhow::{Context, Result};

/// Pull the env's organization. The org_id comes from the env's config in
/// rdc.toml. Returns 1 on success (one organization per env).
pub async fn pull(ctx: &mut PullCtx<'_>, org_id: u64) -> Result<usize> {
    let org = ctx
        .client
        .get_organization(org_id)
        .await
        .with_context(|| format!("fetching organization {org_id}"))?;

    let path = ctx.paths.organization_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let bytes = write_organization(&path, &org)
        .with_context(|| format!("writing organization to {}", path.display()))?;
    let hash = hash_for_lockfile(&bytes);

    record_object(
        ctx.lockfile,
        "organization",
        // Slug for the singleton org is just "self" — there's only one per env,
        // so the slug doesn't appear in the filename. We use a fixed key in the
        // lockfile for symmetry with multi-object kinds.
        "self",
        org.id,
        Some(org.url.clone()),
        org.modified_at().map(|s| s.to_string()),
        Some(hash),
    );

    Ok(1)
}
