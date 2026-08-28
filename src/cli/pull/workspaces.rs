use super::common::{
    PullAction, PullCtx, apply_pull_action, decide_pull_action, portabilize_proposed,
    record_object, skip_on_permission_denied,
};
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
                .base_hash(&value, ctx.lockfile)
                .context("hashing workspace")?;

            record_object(
                ctx.lockfile,
                KIND,
                &slug,
                ws.id,
                ws.modified_at().map(|s| s.to_string()),
                ws.modified_by().map(|s| s.to_string()),
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

/// Same-pass workspace back-ref refresh (idempotency).
///
/// Creating or deleting a queue makes the server update the OTHER side of the
/// link: the owning workspace's server-derived `queues` collection. rdc strips
/// that from every outbound body and never authors it, and the Phase-1 catalog
/// the pull phase consumed predates the push — so on a fresh-env deploy the
/// workspace lands on disk with `"queues": []` and stays that way until the
/// NEXT sync. That makes a single `sync` non-idempotent, which matters most
/// exactly where it is least watched: the unattended CI deploy job.
///
/// `eligible` holds workspace slugs whose on-disk state this cycle is known to
/// equal their recorded base — Clean, or created by this very cycle. The
/// three-way [`decide_pull_action`] is still consulted and anything other than
/// `Write` is skipped, so a local edit can never be clobbered here.
///
/// Returns the number of `workspace.json` files actually rewritten.
pub async fn refresh_backrefs(
    ctx: &mut PullCtx<'_>,
    eligible: &BTreeSet<String>,
    progress: &Arc<Log>,
) -> Result<usize> {
    if eligible.is_empty() {
        return Ok(0);
    }
    let workspaces = list(ctx, progress).await?;
    let mut refreshed = 0usize;
    for ws in &workspaces {
        let Some(slug) = ctx.lockfile.slug_for_id(KIND, ws.id).map(|s| s.to_string()) else {
            continue;
        };
        if !eligible.contains(&slug) {
            continue;
        }
        let ws_path = ctx.paths.workspace_dir(&slug).join("workspace.json");
        if !ws_path.exists() {
            continue;
        }

        let value = serde_json::to_value(ws)?;
        let art = crate::snapshot::codec::codec(KIND)
            .unwrap()
            .disk_bytes(&value)
            .with_context(|| format!("serializing workspace '{slug}' for back-ref refresh"))?;
        let proposed = portabilize_proposed(&art.json, &*ctx.lockfile);

        let base = ctx
            .lockfile
            .objects
            .get(KIND)
            .and_then(|m| m.get(&slug))
            .and_then(|e| e.content_hash.clone());
        let (action, remote_hash) = decide_pull_action(&ws_path, base.as_deref(), &proposed)?;
        if action != PullAction::Write {
            continue;
        }
        let recorded = apply_pull_action(
            action,
            &ws_path,
            &proposed,
            remote_hash,
            ctx.interactive,
            progress,
            ctx.paths.env(),
            base.as_deref(),
            Some(ctx.paths),
        )?;
        record_object(
            ctx.lockfile,
            KIND,
            &slug,
            ws.id,
            ws.modified_at().map(|s| s.to_string()),
            ws.modified_by().map(|s| s.to_string()),
            Some(recorded),
        );
        refreshed += 1;
    }
    if refreshed > 0 {
        progress.event(
            Action::Pull,
            &format!(
                "workspaces ({refreshed} back-ref{} refreshed)",
                if refreshed == 1 { "" } else { "s" }
            ),
        );
    }
    Ok(refreshed)
}
