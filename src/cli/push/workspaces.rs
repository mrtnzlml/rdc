//! Push workspaces. Handles both CREATE (POST) and UPDATE (PATCH).
//! Workspaces sit at the top of the dependency tree — queues, schemas,
//! inboxes, email_templates all root from a workspace by URL — so this
//! driver runs first in the phase-2 dispatch.

use crate::api::RossumClient;
use crate::cli::pull::common::maybe_strip_overlay;
use crate::overlay::{apply_overrides, Overlay};
use crate::paths::Paths;
use crate::progress::{ResourceOp, ResourceOutcome, SyncRenderer};
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
    progress: &Arc<dyn SyncRenderer>,
    env: &str,
) -> Result<(usize, usize)> {
    let overlay = Overlay::load(&paths.overlay_file())
        .with_context(|| format!("loading overlay from {}", paths.overlay_file().display()))?;

    progress.phase("pushing workspaces");
    let mut pushed = 0usize;
    let mut skipped = 0usize;

    for (ws_slug, ws_path) in changes {
        // Workspaces don't have a typed overlay accessor in `Overlay`
        // today (no `[workspaces.<slug>]` section). If users start
        // needing one — e.g. per-env `autopilot` differences — that's a
        // future Overlay extension; for now the payload is sent as-is.
        let overlay_paths: Option<&BTreeMap<String, serde_json::Value>> = None;
        let _ = overlay;

        // CREATE — no lockfile entry yet.
        if lockfile.objects.get("workspaces").and_then(|m| m.get(ws_slug.as_str())).is_none() {
            let disk_bytes = std::fs::read(ws_path)
                .with_context(|| format!("reading {}", ws_path.display()))?;
            let mut payload: serde_json::Value = serde_json::from_slice(&disk_bytes)
                .with_context(|| format!("parsing {}", ws_path.display()))?;
            if let Some(p) = overlay_paths {
                apply_overrides(&mut payload, p);
            }
            strip_for_create(&mut payload, "workspaces");
            progress.resource_started("workspaces", ws_slug, ResourceOp::Post);
            let create_result = client.create_workspace(&payload, Some(progress.clone())).await
                .with_context(|| format!("POST /workspaces (creating '{ws_slug}')"));
            let create_outcome = match &create_result {
                Ok(_) => ResourceOutcome::Ok,
                Err(e) => ResourceOutcome::Failed(e.to_string()),
            };
            progress.resource_finished("workspaces", ws_slug, create_outcome);
            let created = create_result?;
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
                    secrets_hash: None,
                },
            );
            progress.warn_line(&format!("[ok] workspaces/{ws_slug} POST (id {})", created.id));
            pushed += 1;
            continue;
        }

        // UPDATE — existing workspace, PATCH the diff with drift detection.
        let entry = lockfile.objects.get("workspaces").and_then(|m| m.get(ws_slug.as_str())).unwrap();
        let Some(base) = &entry.content_hash else {
            progress.warn_line(&format!("! workspaces/{ws_slug} lockfile entry has no content_hash, skipping"));
            skipped += 1;
            continue;
        };
        let base = base.clone();
        let id = entry.id;

        let disk_bytes = std::fs::read(ws_path)
            .with_context(|| format!("reading {}", ws_path.display()))?;
        let mut payload: serde_json::Value = serde_json::from_slice(&disk_bytes)
            .with_context(|| format!("parsing {}", ws_path.display()))?;
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        let payload_workspace: crate::model::Workspace = serde_json::from_value(payload)
            .with_context(|| format!("deserializing overlay-applied workspace '{ws_slug}'"))?;

        // Drift check.
        let remote_workspace = client.get_workspace(id, Some(progress.clone())).await
            .with_context(|| format!("fetching workspace {id} to verify drift before push"))?;
        let mut remote_bytes = serde_json::to_vec_pretty(&remote_workspace)
            .context("serializing remote workspace")?;
        remote_bytes.push(b'\n');
        let remote_bytes = maybe_strip_overlay(remote_bytes, overlay_paths)?;
        let remote_combined = content_hash(&remote_bytes);
        let mut payload_to_send = payload_workspace;
        if &remote_combined != &base {
            use crate::cli::resolve::{resolve_push_drift, PushDriftOutcome};
            match resolve_push_drift(interactive, ws_path, &remote_bytes, env)? {
                PushDriftOutcome::Patch { payload_override } => {
                    if let Some(bytes) = payload_override {
                        payload_to_send = serde_json::from_slice(&bytes)
                            .with_context(|| format!("re-deserializing edited workspace '{ws_slug}'"))?;
                    }
                }
                PushDriftOutcome::Adopt => {
                    write_atomic(ws_path, &remote_bytes)
                        .with_context(|| format!("adopting remote into {}", ws_path.display()))?;
                    lockfile.upsert(
                        "workspaces",
                        ws_slug,
                        ObjectEntry {
                            id,
                            url: Some(remote_workspace.url.clone()),
                            modified_at: remote_workspace.modified_at().map(|s| s.to_string()),
                            content_hash: Some(remote_combined),
                            secrets_hash: None,
                        },
                    );
                    progress.warn_line(&format!("! workspaces/{ws_slug} adopted remote (drift)"));
                    skipped += 1;
                    continue;
                }
                PushDriftOutcome::Skip => {
                    progress.warn_line(&format!("! workspaces/{ws_slug} remote has changed since last pull, skipping"));
                    skipped += 1;
                    continue;
                }
            }
        }

        progress.resource_started("workspaces", ws_slug, ResourceOp::Patch);
        let patch_result = client.update_workspace(id, &payload_to_send, Some(progress.clone())).await
            .with_context(|| format!("PATCH /workspaces/{id}"));
        let patch_outcome = match &patch_result {
            Ok(_) => ResourceOutcome::Ok,
            Err(e) => ResourceOutcome::Failed(e.to_string()),
        };
        progress.resource_finished("workspaces", ws_slug, patch_outcome);
        let updated = patch_result?;
        let mut updated_bytes = serde_json::to_vec_pretty(&updated)
            .context("serializing updated workspace")?;
        updated_bytes.push(b'\n');
        let updated_bytes = maybe_strip_overlay(updated_bytes, overlay_paths)?;
        let updated_hash = content_hash(&updated_bytes);
        write_atomic(ws_path, &updated_bytes)
            .with_context(|| format!("writing post-push canonical form for '{ws_slug}'"))?;
        lockfile.upsert(
            "workspaces",
            ws_slug,
            ObjectEntry {
                id: updated.id,
                url: Some(updated.url.clone()),
                modified_at: updated.modified_at().map(|s| s.to_string()),
                content_hash: Some(updated_hash),
                secrets_hash: None,
            },
        );
        progress.warn_line(&format!("[ok] workspaces/{ws_slug} PATCH"));
        pushed += 1;
    }

    Ok((pushed, skipped))
}
