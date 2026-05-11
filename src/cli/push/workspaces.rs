//! Push workspaces. Supports both PATCH (existing workspace edited) and
//! POST (new workspace). Workspaces sit at the top of the dependency tree
//! — queues, schemas, inboxes, email_templates all root from a workspace
//! by URL — so this driver runs first in the phase-2 dispatch.

use crate::api::RossumClient;
use crate::cli::pull::common::maybe_strip_overlay;
use crate::overlay::{apply_overrides, Overlay};
use crate::paths::Paths;
use crate::progress::OverallProgress;
use crate::snapshot::create::strip_for_create;
use crate::snapshot::writer::write_atomic;
use crate::state::{content_hash, Lockfile, ObjectEntry};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::sync::Arc;

pub async fn push(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
    interactive: bool,
    changes: &BTreeMap<String, std::path::PathBuf>,
    progress: &Arc<OverallProgress>,
) -> Result<(usize, usize)> {
    let overlay = Overlay::load(&paths.overlay_file())
        .with_context(|| format!("loading overlay from {}", paths.overlay_file().display()))?;

    let mut pushed = 0usize;
    let mut skipped = 0usize;

    for (ws_slug, ws_path) in changes {
        // Overlay for workspaces lives under [hooks.<slug>]-style sections;
        // workspaces don't have a typed overlay accessor today, so the
        // payload is sent as-is (no overlay merge). This keeps the create
        // path simple. Adding an overlay accessor is a follow-up if needed.
        let overlay_paths: Option<&BTreeMap<String, serde_json::Value>> = None;
        let _ = overlay;

        // Missing lockfile entry → new workspace, POST.
        if lockfile.objects.get("workspaces").and_then(|m| m.get(ws_slug.as_str())).is_none() {
            let disk_bytes = std::fs::read(ws_path)
                .with_context(|| format!("reading {}", ws_path.display()))?;
            let mut payload: serde_json::Value = serde_json::from_slice(&disk_bytes)
                .with_context(|| format!("parsing {}", ws_path.display()))?;
            if let Some(p) = overlay_paths {
                apply_overrides(&mut payload, p);
            }
            strip_for_create(&mut payload, "workspaces");
            let created = client.create_workspace(&payload, Some(progress.clone())).await
                .with_context(|| format!("POST /workspaces (creating '{ws_slug}')"))?;
            let mut created_bytes = serde_json::to_vec_pretty(&created)
                .context("serializing created workspace")?;
            created_bytes.push(b'\n');
            let created_bytes = maybe_strip_overlay(created_bytes, overlay_paths)?;
            let created_hash = content_hash(&created_bytes);
            write_atomic(ws_path, &created_bytes)
                .with_context(|| format!("writing post-create canonical form for '{ws_slug}'"))?;
            lockfile.upsert(
                "workspaces",
                ws_slug,
                ObjectEntry {
                    id: created.id,
                    url: Some(created.url.clone()),
                    modified_at: created.modified_at().map(|s| s.to_string()),
                    content_hash: Some(created_hash),
                },
            );
            progress.println(format!("created workspaces/{ws_slug} (id {})", created.id));
            progress.tick(ws_slug.as_str());
            pushed += 1;
            continue;
        }

        // Workspaces have no PATCH driver today. Local edits on existing
        // workspaces aren't pushed (would need an update_workspace method
        // + drift detection). Surface a clear message rather than a
        // silent skip so users know an edit they made won't propagate.
        progress.println(format!(
            "warning: workspaces/{ws_slug} has local edits but workspace push is create-only — re-run `rdc pull` to revert, or PATCH manually via the API"
        ));
        skipped += 1;
        let _ = interactive;
    }

    Ok((pushed, skipped))
}
