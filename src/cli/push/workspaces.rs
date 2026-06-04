//! Push workspaces. Handles both CREATE (POST) and UPDATE (PATCH).
//! Workspaces sit at the top of the dependency tree — queues, schemas,
//! inboxes, email_templates all root from a workspace by URL — so this
//! driver runs first in the phase-2 dispatch.

use crate::api::RossumClient;
use crate::cli::pull::common::maybe_strip_overlay;
use crate::log::{Action, Log};
use crate::overlay::{Overlay, apply_overrides};
use crate::paths::Paths;

use crate::snapshot::codec::combined_hash;
use crate::snapshot::create::{strip_for_create, strip_patch_extra};
use crate::snapshot::writer::write_atomic;
use crate::state::{Lockfile, ObjectEntry};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::sync::Arc;

pub async fn push(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
    interactive: bool,
    changes: &BTreeMap<String, std::path::PathBuf>,
    progress: &Arc<Log>,
    env: &str,
) -> Result<(usize, usize)> {
    let overlay = Overlay::load(&paths.overlay_file())
        .with_context(|| format!("loading overlay from {}", paths.overlay_file().display()))?;

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
        if lockfile
            .objects
            .get("workspaces")
            .and_then(|m| m.get(ws_slug.as_str()))
            .is_none()
        {
            let disk_bytes =
                std::fs::read(ws_path).with_context(|| format!("reading {}", ws_path.display()))?;
            let mut payload: serde_json::Value = serde_json::from_slice(&disk_bytes)
                .with_context(|| format!("parsing {}", ws_path.display()))?;
            if let Some(p) = overlay_paths {
                apply_overrides(&mut payload, p);
            }
            strip_for_create(&mut payload, "workspaces");
            let create_result = client
                .create_workspace(&payload, Some(progress.clone()))
                .await
                .with_context(|| format!("POST /workspaces (creating '{ws_slug}')"));
            let created = create_result?;
            let codec = crate::snapshot::codec::codec("workspaces").unwrap();
            let created_art = codec
                .disk_bytes(
                    &serde_json::to_value(&created).context("serializing created workspace")?,
                )
                .context("codec disk_bytes for created workspace")?;
            let created_bytes = maybe_strip_overlay(created_art.json, overlay_paths)?;
            let created_hash = combined_hash(&created_bytes, &created_art.sidecars);
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
            progress.event(
                Action::Post,
                &format!("workspace/{ws_slug} id={}", created.id),
            );
            pushed += 1;
            continue;
        }

        // UPDATE — existing workspace, PATCH the diff with drift detection.
        let entry = lockfile
            .objects
            .get("workspaces")
            .and_then(|m| m.get(ws_slug.as_str()))
            .unwrap();
        let Some(base) = &entry.content_hash else {
            progress.event(
                Action::Skip,
                &format!("workspace/{ws_slug} (no content_hash)"),
            );
            skipped += 1;
            continue;
        };
        let base = base.clone();
        let id = entry.id;

        let disk_bytes =
            std::fs::read(ws_path).with_context(|| format!("reading {}", ws_path.display()))?;
        let mut payload: serde_json::Value = serde_json::from_slice(&disk_bytes)
            .with_context(|| format!("parsing {}", ws_path.display()))?;
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        let payload_workspace: crate::model::Workspace = serde_json::from_value(payload)
            .with_context(|| format!("deserializing overlay-applied workspace '{ws_slug}'"))?;

        // Drift check.
        let remote_workspace = client
            .get_workspace(id, Some(progress.clone()))
            .await
            .with_context(|| format!("fetching workspace {id} to verify drift before push"))?;
        let codec = crate::snapshot::codec::codec("workspaces").unwrap();
        let remote_art = codec
            .disk_bytes(
                &serde_json::to_value(&remote_workspace)
                    .context("serializing remote workspace for drift check")?,
            )
            .context("codec disk_bytes for remote workspace")?;
        let remote_bytes = maybe_strip_overlay(remote_art.json, overlay_paths)?;
        let remote_combined = combined_hash(&remote_bytes, &remote_art.sidecars);
        let mut payload_to_send = payload_workspace;
        if remote_combined != base {
            use crate::cli::resolve::{PushDriftOutcome, resolve_push_drift};
            match resolve_push_drift(interactive, ws_path, &remote_bytes, env)? {
                PushDriftOutcome::Patch { payload_override } => {
                    if let Some(bytes) = payload_override {
                        payload_to_send = serde_json::from_slice(&bytes).with_context(|| {
                            format!("re-deserializing edited workspace '{ws_slug}'")
                        })?;
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
                    progress.event(
                        Action::Warn,
                        &format!("workspace/{ws_slug} adopted remote (drift)"),
                    );
                    skipped += 1;
                    continue;
                }
                PushDriftOutcome::Skip => {
                    progress.event(
                        Action::Skip,
                        &format!("workspace/{ws_slug} (remote changed; rdc sync first)"),
                    );
                    skipped += 1;
                    continue;
                }
            }
        }

        // Strip server-managed fields from `extra` so the PATCH matches the
        // CREATE contract (the server-computed `queues` back-ref).
        strip_patch_extra(&mut payload_to_send.extra, "workspaces", false);
        let patch_result = client
            .update_workspace(id, &payload_to_send, Some(progress.clone()))
            .await
            .with_context(|| format!("PATCH /workspaces/{id}"));
        let updated = patch_result?;
        let codec = crate::snapshot::codec::codec("workspaces").unwrap();
        let updated_art = codec
            .disk_bytes(
                &serde_json::to_value(&updated)
                    .context("serializing updated workspace for disk write")?,
            )
            .context("codec disk_bytes for updated workspace")?;
        let updated_bytes = maybe_strip_overlay(updated_art.json, overlay_paths)?;
        let updated_hash = combined_hash(&updated_bytes, &updated_art.sidecars);
        crate::state::base_cache::write_disk_and_cache(paths, ws_path, &updated_bytes)
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
        progress.event(Action::Patch, &format!("workspace/{ws_slug}"));
        pushed += 1;
    }

    Ok((pushed, skipped))
}
