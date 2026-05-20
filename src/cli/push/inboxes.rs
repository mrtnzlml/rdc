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

    progress.phase("pushing inboxes");
    let mut pushed = 0usize;
    let mut skipped = 0usize;

    for (q_slug, inbox_path) in changes {
        let overlay_paths = overlay.as_ref().and_then(|ov| ov.inbox(q_slug));

        // Missing lockfile entry → new inbox, POST.
        if lockfile.objects.get("inboxes").and_then(|m| m.get(q_slug.as_str())).is_none() {
            let disk_bytes = std::fs::read(inbox_path)
                .with_context(|| format!("reading {}", inbox_path.display()))?;
            let mut payload: serde_json::Value = serde_json::from_slice(&disk_bytes)
                .with_context(|| format!("parsing {}", inbox_path.display()))?;
            if let Some(p) = overlay_paths {
                apply_overrides(&mut payload, p);
            }
            strip_for_create(&mut payload, "inboxes");
            progress.resource_started("inboxes", q_slug, ResourceOp::Post);
            let create_result = client.create_inbox(&payload, Some(progress.clone())).await
                .with_context(|| format!("POST /inboxes (creating for queue '{q_slug}')"));
            let create_outcome = match &create_result {
                Ok(_) => ResourceOutcome::Ok,
                Err(e) => ResourceOutcome::Failed(e.to_string()),
            };
            progress.resource_finished("inboxes", q_slug, create_outcome);
            let created = create_result?;
            let mut created_bytes = serde_json::to_vec_pretty(&created)
                .context("serializing created inbox")?;
            created_bytes.push(b'\n');
            let created_bytes = maybe_strip_overlay(created_bytes, overlay_paths)?;
            let created_hash = content_hash(&created_bytes);
            write_atomic(inbox_path, &created_bytes)
                .with_context(|| format!("writing post-create canonical form for inbox '{q_slug}'"))?;
            lockfile.upsert(
                "inboxes",
                q_slug,
                ObjectEntry {
                    id: created.id,
                    url: Some(created.url.clone()),
                    modified_at: created.modified_at().map(|s| s.to_string()),
                    content_hash: Some(created_hash),
                    secrets_hash: None,
                },
            );
            progress.warn_line(&format!("[ok] inboxes/{q_slug} POST (id {})", created.id));
            pushed += 1;
            continue;
        }

        let disk_bytes = std::fs::read(inbox_path)
            .with_context(|| format!("reading {}", inbox_path.display()))?;
        let entry = lockfile.objects.get("inboxes").and_then(|m| m.get(q_slug.as_str())).unwrap();
        let Some(base) = &entry.content_hash else {
            progress.warn_line(&format!("! inboxes/{q_slug} lockfile entry has no content_hash, skipping"));
            skipped += 1;
            continue;
        };
        let base = base.clone();

        let mut payload: serde_json::Value = serde_json::from_slice(&disk_bytes)
            .with_context(|| format!("parsing {}", inbox_path.display()))?;
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        let payload_inbox: crate::model::Inbox = serde_json::from_value(payload)
            .with_context(|| format!("deserializing overlay-applied inbox '{q_slug}'"))?;

        let id = entry.id;
        let remote_inbox = client.get_inbox(id, Some(progress.clone())).await
            .with_context(|| format!("fetching inbox {id} to verify drift before push"))?;
        let mut remote_bytes = serde_json::to_vec_pretty(&remote_inbox)
            .context("serializing remote inbox")?;
        remote_bytes.push(b'\n');
        let remote_bytes = maybe_strip_overlay(remote_bytes, overlay_paths)?;
        let remote_combined = content_hash(&remote_bytes);
        let mut payload_to_send = payload_inbox;
        if &remote_combined != &base {
            use crate::cli::resolve::{resolve_push_drift, PushDriftOutcome};
            match resolve_push_drift(interactive, inbox_path, &remote_bytes, env)? {
                PushDriftOutcome::Patch { payload_override } => {
                    if let Some(bytes) = payload_override {
                        payload_to_send = serde_json::from_slice(&bytes)
                            .with_context(|| format!("re-deserializing edited inbox '{q_slug}'"))?;
                    }
                }
                PushDriftOutcome::Adopt => {
                    write_atomic(inbox_path, &remote_bytes)
                        .with_context(|| format!("adopting remote into {}", inbox_path.display()))?;
                    lockfile.upsert(
                        "inboxes",
                        q_slug,
                        ObjectEntry {
                            id,
                            url: Some(remote_inbox.url.clone()),
                            modified_at: remote_inbox.modified_at().map(|s| s.to_string()),
                            content_hash: Some(remote_combined),
                            secrets_hash: None,
                        },
                    );
                    progress.warn_line(&format!("! inboxes/{q_slug} adopted remote (drift)"));
                    skipped += 1;
                    continue;
                }
                PushDriftOutcome::Skip => {
                    progress.warn_line(&format!("! inboxes/{q_slug} remote has changed since last sync, skipping push (run `rdc sync` first)"));
                    skipped += 1;
                    continue;
                }
            }
        }

        progress.resource_started("inboxes", q_slug, ResourceOp::Patch);
        let patch_result = client.update_inbox(id, &payload_to_send, Some(progress.clone())).await
            .with_context(|| format!("PATCH /inboxes/{id}"));
        let patch_outcome = match &patch_result {
            Ok(_) => ResourceOutcome::Ok,
            Err(e) => ResourceOutcome::Failed(e.to_string()),
        };
        progress.resource_finished("inboxes", q_slug, patch_outcome);
        let updated = patch_result?;

        let mut updated_bytes = serde_json::to_vec_pretty(&updated)
            .context("serializing updated inbox")?;
        updated_bytes.push(b'\n');
        let updated_bytes = maybe_strip_overlay(updated_bytes, overlay_paths)?;
        let updated_hash = content_hash(&updated_bytes);
        write_atomic(inbox_path, &updated_bytes)
            .with_context(|| format!("writing post-push canonical form for inbox '{q_slug}'"))?;

        lockfile.upsert(
            "inboxes",
            q_slug,
            ObjectEntry {
                id: updated.id,
                url: Some(updated.url.clone()),
                modified_at: updated.modified_at().map(|s| s.to_string()),
                content_hash: Some(updated_hash),
                secrets_hash: None,
            },
        );
        progress.warn_line(&format!("[ok] inboxes/{q_slug} PATCH"));
        pushed += 1;
    }

    Ok((pushed, skipped))
}
