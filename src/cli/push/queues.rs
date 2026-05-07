use crate::api::RossumClient;
use crate::cli::pull::common::maybe_strip_overlay;
use crate::overlay::{apply_overrides, Overlay};
use crate::paths::Paths;
use crate::snapshot::writer::write_atomic;
use crate::state::{content_hash, Lockfile, ObjectEntry};
use anyhow::{Context, Result};

pub async fn push(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
) -> Result<(usize, usize)> {
    let workspaces_dir = paths.workspaces_dir();
    if !workspaces_dir.exists() {
        return Ok((0, 0));
    }

    let overlay = Overlay::load(&paths.overlay_file())
        .with_context(|| format!("loading overlay from {}", paths.overlay_file().display()))?;

    let mut pushed = 0usize;
    let mut skipped = 0usize;
    let mut remote_cache: std::collections::HashMap<u64, crate::model::Queue> =
        std::collections::HashMap::new();

    for ws_entry in std::fs::read_dir(&workspaces_dir)
        .with_context(|| format!("reading {}", workspaces_dir.display()))?
    {
        let ws_entry = ws_entry?;
        if !ws_entry.file_type()?.is_dir() {
            continue;
        }
        let ws_slug = ws_entry.file_name().to_string_lossy().to_string();
        let queues_dir = paths.queues_dir(&ws_slug);
        if !queues_dir.exists() {
            continue;
        }

        for q_entry in std::fs::read_dir(&queues_dir)
            .with_context(|| format!("reading {}", queues_dir.display()))?
        {
            let q_entry = q_entry?;
            if !q_entry.file_type()?.is_dir() {
                continue;
            }
            let q_slug = q_entry.file_name().to_string_lossy().to_string();
            let queue_dir = paths.queue_dir(&ws_slug, &q_slug);
            let queue_path = queue_dir.join("queue.json");
            if !queue_path.exists() {
                continue;
            }

            let disk_bytes = std::fs::read(&queue_path)
                .with_context(|| format!("reading {}", queue_path.display()))?;
            let local_combined = content_hash(&disk_bytes);

            let entry = lockfile.objects.get("queues").and_then(|m| m.get(&q_slug));
            let Some(entry) = entry else {
                eprintln!("warning: queue '{q_slug}' — no lockfile entry, skipping");
                skipped += 1;
                continue;
            };
            let Some(base) = &entry.content_hash else {
                eprintln!("warning: queue '{q_slug}' — lockfile entry has no content_hash, skipping");
                skipped += 1;
                continue;
            };
            if &local_combined == base {
                continue;
            }

            let mut payload: serde_json::Value = serde_json::from_slice(&disk_bytes)
                .with_context(|| format!("parsing {}", queue_path.display()))?;
            let overlay_paths = overlay.as_ref().and_then(|ov| ov.queue(&q_slug));
            if let Some(p) = overlay_paths {
                apply_overrides(&mut payload, p);
            }
            let payload_queue: crate::model::Queue = serde_json::from_value(payload)
                .with_context(|| format!("deserializing overlay-applied queue '{q_slug}'"))?;

            let id = entry.id;
            if remote_cache.is_empty() {
                let remotes = client.list_queues().await
                    .context("listing queues to verify no drift before push")?;
                for r in remotes {
                    remote_cache.insert(r.id, r);
                }
            }
            let Some(remote_queue) = remote_cache.get(&id).cloned() else {
                eprintln!("warning: queue '{q_slug}' — id {id} not found on remote, skipping");
                skipped += 1;
                continue;
            };
            let mut remote_bytes = serde_json::to_vec_pretty(&remote_queue)
                .context("serializing remote queue")?;
            remote_bytes.push(b'\n');
            let remote_bytes = maybe_strip_overlay(remote_bytes, overlay_paths)?;
            let remote_combined = content_hash(&remote_bytes);
            if &remote_combined != base {
                eprintln!(
                    "warning: queue '{q_slug}' — remote has changed since last pull, skipping push (run `rdc pull` first)"
                );
                skipped += 1;
                continue;
            }

            let updated = client.update_queue(id, &payload_queue).await
                .with_context(|| format!("PATCH /queues/{id}"))?;

            let mut updated_bytes = serde_json::to_vec_pretty(&updated)
                .context("serializing updated queue")?;
            updated_bytes.push(b'\n');
            let updated_bytes = maybe_strip_overlay(updated_bytes, overlay_paths)?;
            let updated_hash = content_hash(&updated_bytes);
            write_atomic(&queue_path, &updated_bytes)
                .with_context(|| format!("writing post-push canonical form for queue '{q_slug}'"))?;

            lockfile.upsert(
                "queues",
                &q_slug,
                ObjectEntry {
                    id: updated.id,
                    url: Some(updated.url.clone()),
                    modified_at: updated.modified_at().map(|s| s.to_string()),
                    content_hash: Some(updated_hash),
                },
            );
            pushed += 1;
        }
    }

    Ok((pushed, skipped))
}
