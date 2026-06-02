use super::common::{PullCtx, record_object, skip_on_permission_denied};
use crate::log::{Action, Log};
use crate::model::Workspace;
use crate::slug::slugify_unique;
use crate::snapshot::writer::write_atomic;
use anyhow::{Context, Result};
use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

const KIND: &str = "workspaces";

/// Phase 1: list all workspaces from the API.
pub async fn list(ctx: &PullCtx<'_>, progress: &Arc<Log>) -> Result<Vec<Workspace>> {
    skip_on_permission_denied(
        ctx.client
            .list_workspaces(Some(progress.clone()))
            .await
            .context("listing workspaces"),
        KIND,
        progress,
    )
}

/// Phase 2: write listed workspaces to disk. `subset` selects which
/// `(kind, slug)` pairs are actually written; items outside the subset are
/// skipped silently. Returns the number written.
pub async fn process(
    ctx: &mut PullCtx<'_>,
    workspaces: Vec<Workspace>,
    subset: &BTreeSet<(String, String)>,
    progress: &Arc<Log>,
) -> Result<usize> {
    let mut used_slugs: HashSet<String> = HashSet::new();
    let mut dir_created = false;
    let mut count = 0usize;
    for ws in &workspaces {
        let slug = match ctx.lockfile.slug_for_id(KIND, ws.id) {
            Some(existing) => existing.to_string(),
            None => slugify_unique(&ws.name, &used_slugs),
        };
        used_slugs.insert(slug.clone());

        if !subset.contains(&(KIND.to_string(), slug.clone())) {
            continue;
        }

        let result: Result<()> = (|| {
            if !dir_created {
                std::fs::create_dir_all(ctx.paths.workspaces_dir()).with_context(|| {
                    format!("creating {}", ctx.paths.workspaces_dir().display())
                })?;
                dir_created = true;
            }

            let ws_dir = ctx.paths.workspace_dir(&slug);
            std::fs::create_dir_all(&ws_dir)
                .with_context(|| format!("creating {}", ws_dir.display()))?;

            // Canonical on-disk bytes via KindCodec: strips `modified_at`.
            // No overlay for workspaces.
            let value = serde_json::to_value(ws)?;
            let art = crate::snapshot::codec::codec(KIND)
                .unwrap()
                .disk_bytes(&value)
                .context("serializing workspace")?;
            let bytes = art.json;

            let ws_path = ws_dir.join("workspace.json");
            write_atomic(&ws_path, &bytes)
                .with_context(|| format!("writing {}", ws_path.display()))?;
            // Mirror the just-written bytes to the base cache so the next
            // sync's 3-way merge has a current merge base.
            crate::state::base_cache::write(ctx.paths, &ws_path, &bytes)?;

            let hash = crate::snapshot::codec::codec(KIND)
                .unwrap()
                .base_hash(&value)
                .context("hashing workspace")?;

            record_object(
                ctx.lockfile,
                KIND,
                &slug,
                ws.id,
                Some(ws.url.clone()),
                ws.modified_at().map(|s| s.to_string()),
                Some(hash),
            );

            count += 1;
            Ok(())
        })();
        result?;
    }

    if count > 0 {
        progress.event(Action::Pull, &format!("workspaces ({count} pulled)"));
    }

    Ok(count)
}
