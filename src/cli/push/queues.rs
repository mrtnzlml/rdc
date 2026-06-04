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
    let mut remote_cache: std::collections::HashMap<u64, crate::model::Queue> =
        std::collections::HashMap::new();

    for (q_slug, queue_path) in changes {
        let overlay_paths = overlay.as_ref().and_then(|ov| ov.queue(q_slug));

        // Missing lockfile entry → new queue, POST. User must already have
        // POSTed the referenced workspace + schema (linear push); if not,
        // the server will reject with a clear error.
        if lockfile
            .objects
            .get("queues")
            .and_then(|m| m.get(q_slug.as_str()))
            .is_none()
        {
            let disk_bytes = std::fs::read(queue_path)
                .with_context(|| format!("reading {}", queue_path.display()))?;
            let mut payload: serde_json::Value = serde_json::from_slice(&disk_bytes)
                .with_context(|| format!("parsing {}", queue_path.display()))?;
            if let Some(p) = overlay_paths {
                apply_overrides(&mut payload, p);
            }
            strip_for_create(&mut payload, "queues");
            let create_result = client
                .create_queue(&payload, Some(progress.clone()))
                .await
                .with_context(|| format!("POST /queues (creating '{q_slug}')"));
            let created = create_result?;
            // Canonical on-disk bytes via KindCodec: redacts `counts` and
            // strips hidden fields — matching exactly what pull produces.
            let codec = crate::snapshot::codec::codec("queues").unwrap();
            let created_art = codec
                .disk_bytes(&serde_json::to_value(&created).context("serializing created queue")?)
                .context("codec disk_bytes for created queue")?;
            let created_bytes = maybe_strip_overlay(created_art.json, overlay_paths)?;
            let created_hash = combined_hash(&created_bytes, &created_art.sidecars);
            write_atomic(queue_path, &created_bytes)
                .with_context(|| format!("writing post-create canonical form for '{q_slug}'"))?;
            lockfile.upsert(
                "queues",
                q_slug,
                ObjectEntry {
                    id: created.id,
                    url: Some(created.url.clone()),
                    modified_at: created.modified_at().map(|s| s.to_string()),
                    content_hash: Some(created_hash),
                    secrets_hash: None,
                },
            );
            progress.event(Action::Post, &format!("queue/{q_slug} id={}", created.id));
            pushed += 1;
            continue;
        }

        let disk_bytes = std::fs::read(queue_path)
            .with_context(|| format!("reading {}", queue_path.display()))?;
        let entry = lockfile
            .objects
            .get("queues")
            .and_then(|m| m.get(q_slug.as_str()))
            .unwrap();
        let Some(base) = &entry.content_hash else {
            progress.event(Action::Skip, &format!("queue/{q_slug} (no content_hash)"));
            skipped += 1;
            continue;
        };
        let base = base.clone();

        let mut payload: serde_json::Value = serde_json::from_slice(&disk_bytes)
            .with_context(|| format!("parsing {}", queue_path.display()))?;
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        let payload_queue: crate::model::Queue = serde_json::from_value(payload)
            .with_context(|| format!("deserializing overlay-applied queue '{q_slug}'"))?;

        let id = entry.id;
        if remote_cache.is_empty() {
            let remotes = client
                .list_queues(Some(progress.clone()))
                .await
                .context("listing queues to verify no drift before push")?;
            for r in remotes {
                remote_cache.insert(r.id, r);
            }
        }
        let Some(remote_queue) = remote_cache.get(&id).cloned() else {
            progress.event(
                Action::Skip,
                &format!("queue/{q_slug} (remote id {id} missing)"),
            );
            skipped += 1;
            continue;
        };
        // Drift check: use KindCodec so the hash matches the lockfile baseline
        // (redacts `counts`, strips hidden fields).
        let codec = crate::snapshot::codec::codec("queues").unwrap();
        let remote_art = codec
            .disk_bytes(
                &serde_json::to_value(&remote_queue)
                    .context("serializing remote queue for drift check")?,
            )
            .context("codec disk_bytes for remote queue")?;
        let remote_bytes = maybe_strip_overlay(remote_art.json, overlay_paths)?;
        let remote_combined = combined_hash(&remote_bytes, &remote_art.sidecars);
        let mut payload_to_send = payload_queue;
        if remote_combined != base {
            use crate::cli::resolve::{PushDriftOutcome, resolve_push_drift};
            match resolve_push_drift(interactive, queue_path, &remote_bytes, env)? {
                PushDriftOutcome::Patch { payload_override } => {
                    if let Some(bytes) = payload_override {
                        payload_to_send = serde_json::from_slice(&bytes)
                            .with_context(|| format!("re-deserializing edited queue '{q_slug}'"))?;
                    }
                }
                PushDriftOutcome::Adopt => {
                    write_atomic(queue_path, &remote_bytes).with_context(|| {
                        format!("adopting remote into {}", queue_path.display())
                    })?;
                    lockfile.upsert(
                        "queues",
                        q_slug,
                        ObjectEntry {
                            id,
                            url: Some(remote_queue.url.clone()),
                            modified_at: remote_queue.modified_at().map(|s| s.to_string()),
                            content_hash: Some(remote_combined),
                            secrets_hash: None,
                        },
                    );
                    progress.event(
                        Action::Warn,
                        &format!("queue/{q_slug} adopted remote (drift)"),
                    );
                    skipped += 1;
                    continue;
                }
                PushDriftOutcome::Skip => {
                    progress.event(
                        Action::Skip,
                        &format!("queue/{q_slug} (remote changed; rdc sync first)"),
                    );
                    skipped += 1;
                    continue;
                }
            }
        }

        // Strip server-managed fields from `extra` so the PATCH matches the
        // CREATE contract. Critically, `rir_url` is a per-cluster internal
        // service URL the API 400s ("Invalid URL") if echoed back, and
        // `counts` is redacted to the sentinel on disk.
        strip_patch_extra(&mut payload_to_send.extra, "queues", false);
        let patch_result = client
            .update_queue(id, &payload_to_send, Some(progress.clone()))
            .await
            .with_context(|| format!("PATCH /queues/{id}"));
        let updated = patch_result?;

        let codec = crate::snapshot::codec::codec("queues").unwrap();
        let updated_art = codec
            .disk_bytes(
                &serde_json::to_value(&updated)
                    .context("serializing updated queue for disk write")?,
            )
            .context("codec disk_bytes for updated queue")?;
        let updated_bytes = maybe_strip_overlay(updated_art.json, overlay_paths)?;
        let updated_hash = combined_hash(&updated_bytes, &updated_art.sidecars);
        crate::state::base_cache::write_disk_and_cache(paths, queue_path, &updated_bytes)
            .with_context(|| format!("writing post-push canonical form for queue '{q_slug}'"))?;

        lockfile.upsert(
            "queues",
            q_slug,
            ObjectEntry {
                id: updated.id,
                url: Some(updated.url.clone()),
                modified_at: updated.modified_at().map(|s| s.to_string()),
                content_hash: Some(updated_hash),
                secrets_hash: None,
            },
        );
        progress.event(Action::Patch, &format!("queue/{q_slug}"));
        pushed += 1;
    }

    Ok((pushed, skipped))
}
