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

    for (q_slug, inbox_path) in changes {
        let overlay_paths = overlay.as_ref().and_then(|ov| ov.inbox(q_slug));

        // Missing lockfile entry → new inbox, POST.
        if lockfile
            .objects
            .get("inboxes")
            .and_then(|m| m.get(q_slug.as_str()))
            .is_none()
        {
            let disk_bytes = std::fs::read(inbox_path)
                .with_context(|| format!("reading {}", inbox_path.display()))?;
            let mut payload: serde_json::Value = serde_json::from_slice(&disk_bytes)
                .with_context(|| format!("parsing {}", inbox_path.display()))?;
            if let Some(p) = overlay_paths {
                apply_overrides(&mut payload, p);
            }
            strip_for_create(&mut payload, "inboxes");
            let create_result = client
                .create_inbox(&payload, Some(progress.clone()))
                .await
                .with_context(|| format!("POST /inboxes (creating for queue '{q_slug}')"));
            let created = create_result?;
            let codec = crate::snapshot::codec::codec("inboxes").unwrap();
            let created_art = codec
                .disk_bytes(&serde_json::to_value(&created).context("serializing created inbox")?)
                .context("codec disk_bytes for created inbox")?;
            let created_bytes = maybe_strip_overlay(created_art.json, overlay_paths)?;
            let created_hash = combined_hash(&created_bytes, &created_art.sidecars);
            write_atomic(inbox_path, &created_bytes).with_context(|| {
                format!("writing post-create canonical form for inbox '{q_slug}'")
            })?;
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
            progress.event(Action::Post, &format!("inbox/{q_slug} id={}", created.id));
            pushed += 1;
            continue;
        }

        let disk_bytes = std::fs::read(inbox_path)
            .with_context(|| format!("reading {}", inbox_path.display()))?;
        let entry = lockfile
            .objects
            .get("inboxes")
            .and_then(|m| m.get(q_slug.as_str()))
            .unwrap();
        let Some(base) = &entry.content_hash else {
            progress.event(Action::Skip, &format!("inbox/{q_slug} (no content_hash)"));
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
        let remote_inbox = client
            .get_inbox(id, Some(progress.clone()))
            .await
            .with_context(|| format!("fetching inbox {id} to verify drift before push"))?;
        let codec = crate::snapshot::codec::codec("inboxes").unwrap();
        let remote_art = codec
            .disk_bytes(
                &serde_json::to_value(&remote_inbox)
                    .context("serializing remote inbox for drift check")?,
            )
            .context("codec disk_bytes for remote inbox")?;
        let remote_bytes = maybe_strip_overlay(remote_art.json, overlay_paths)?;
        let remote_combined = combined_hash(&remote_bytes, &remote_art.sidecars);
        let mut payload_to_send = payload_inbox;
        if remote_combined != base {
            use crate::cli::resolve::{PushDriftOutcome, resolve_push_drift};
            match resolve_push_drift(interactive, inbox_path, &remote_bytes, env)? {
                PushDriftOutcome::Patch { payload_override } => {
                    if let Some(bytes) = payload_override {
                        payload_to_send = serde_json::from_slice(&bytes)
                            .with_context(|| format!("re-deserializing edited inbox '{q_slug}'"))?;
                    }
                }
                PushDriftOutcome::Adopt => {
                    write_atomic(inbox_path, &remote_bytes).with_context(|| {
                        format!("adopting remote into {}", inbox_path.display())
                    })?;
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
                    progress.event(
                        Action::Warn,
                        &format!("inbox/{q_slug} adopted remote (drift)"),
                    );
                    skipped += 1;
                    continue;
                }
                PushDriftOutcome::Skip => {
                    progress.event(
                        Action::Skip,
                        &format!("inbox/{q_slug} (remote changed; rdc sync first)"),
                    );
                    skipped += 1;
                    continue;
                }
            }
        }

        // Strip server-managed fields from `extra` so the PATCH matches the
        // CREATE contract (`email` is server-assigned).
        strip_patch_extra(&mut payload_to_send.extra, "inboxes", false);
        let patch_result = client
            .update_inbox(id, &payload_to_send, Some(progress.clone()))
            .await
            .with_context(|| format!("PATCH /inboxes/{id}"));
        let updated = patch_result?;

        let codec = crate::snapshot::codec::codec("inboxes").unwrap();
        let updated_art = codec
            .disk_bytes(
                &serde_json::to_value(&updated)
                    .context("serializing updated inbox for disk write")?,
            )
            .context("codec disk_bytes for updated inbox")?;
        let updated_bytes = maybe_strip_overlay(updated_art.json, overlay_paths)?;
        let updated_hash = combined_hash(&updated_bytes, &updated_art.sidecars);
        crate::state::base_cache::write_disk_and_cache(paths, inbox_path, &updated_bytes)
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
        progress.event(Action::Patch, &format!("inbox/{q_slug}"));
        pushed += 1;
    }

    Ok((pushed, skipped))
}
